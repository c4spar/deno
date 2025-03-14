// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use crate::bindings;
use crate::error::generic_error;
use crate::extensions::ExtensionFileSource;
use crate::module_specifier::ModuleSpecifier;
use crate::resolve_import;
use crate::resolve_url;
use crate::JsRuntime;
use crate::OpState;
use anyhow::Error;
use futures::future::FutureExt;
use futures::stream::FuturesUnordered;
use futures::stream::Stream;
use futures::stream::StreamFuture;
use futures::stream::TryStreamExt;
use log::debug;
use serde::Deserialize;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;

pub type ModuleId = usize;
pub(crate) type ModuleLoadId = i32;

pub const BOM_CHAR: &[u8] = &[0xef, 0xbb, 0xbf];

/// Strips the byte order mark from the provided text if it exists.
fn strip_bom(source_code: &[u8]) -> &[u8] {
  if source_code.starts_with(BOM_CHAR) {
    &source_code[BOM_CHAR.len()..]
  } else {
    source_code
  }
}

const SUPPORTED_TYPE_ASSERTIONS: &[&str] = &["json"];

/// Throws V8 exception if assertions are invalid
pub(crate) fn validate_import_assertions(
  scope: &mut v8::HandleScope,
  assertions: &HashMap<String, String>,
) {
  for (key, value) in assertions {
    if key == "type" && !SUPPORTED_TYPE_ASSERTIONS.contains(&value.as_str()) {
      let message = v8::String::new(
        scope,
        &format!("\"{value}\" is not a valid module type."),
      )
      .unwrap();
      let exception = v8::Exception::type_error(scope, message);
      scope.throw_exception(exception);
      return;
    }
  }
}

#[derive(Debug)]
pub(crate) enum ImportAssertionsKind {
  StaticImport,
  DynamicImport,
}

pub(crate) fn parse_import_assertions(
  scope: &mut v8::HandleScope,
  import_assertions: v8::Local<v8::FixedArray>,
  kind: ImportAssertionsKind,
) -> HashMap<String, String> {
  let mut assertions: HashMap<String, String> = HashMap::default();

  let assertions_per_line = match kind {
    // For static imports, assertions are triples of (keyword, value and source offset)
    // Also used in `module_resolve_callback`.
    ImportAssertionsKind::StaticImport => 3,
    // For dynamic imports, assertions are tuples of (keyword, value)
    ImportAssertionsKind::DynamicImport => 2,
  };
  assert_eq!(import_assertions.length() % assertions_per_line, 0);
  let no_of_assertions = import_assertions.length() / assertions_per_line;

  for i in 0..no_of_assertions {
    let assert_key = import_assertions
      .get(scope, assertions_per_line * i)
      .unwrap();
    let assert_key_val = v8::Local::<v8::Value>::try_from(assert_key).unwrap();
    let assert_value = import_assertions
      .get(scope, (assertions_per_line * i) + 1)
      .unwrap();
    let assert_value_val =
      v8::Local::<v8::Value>::try_from(assert_value).unwrap();
    assertions.insert(
      assert_key_val.to_rust_string_lossy(scope),
      assert_value_val.to_rust_string_lossy(scope),
    );
  }

  assertions
}

pub(crate) fn get_asserted_module_type_from_assertions(
  assertions: &HashMap<String, String>,
) -> AssertedModuleType {
  assertions
    .get("type")
    .map(|ty| {
      if ty == "json" {
        AssertedModuleType::Json
      } else {
        AssertedModuleType::JavaScriptOrWasm
      }
    })
    .unwrap_or(AssertedModuleType::JavaScriptOrWasm)
}

// Clippy thinks the return value doesn't need to be an Option, it's unaware
// of the mapping that MapFnFrom<F> does for ResolveModuleCallback.
#[allow(clippy::unnecessary_wraps)]
fn json_module_evaluation_steps<'a>(
  context: v8::Local<'a, v8::Context>,
  module: v8::Local<v8::Module>,
) -> Option<v8::Local<'a, v8::Value>> {
  // SAFETY: `CallbackScope` can be safely constructed from `Local<Context>`
  let scope = &mut unsafe { v8::CallbackScope::new(context) };
  let tc_scope = &mut v8::TryCatch::new(scope);
  let module_map = JsRuntime::module_map(tc_scope);

  let handle = v8::Global::<v8::Module>::new(tc_scope, module);
  let value_handle = module_map
    .borrow_mut()
    .json_value_store
    .remove(&handle)
    .unwrap();
  let value_local = v8::Local::new(tc_scope, value_handle);

  let name = v8::String::new(tc_scope, "default").unwrap();
  // This should never fail
  assert!(
    module.set_synthetic_module_export(tc_scope, name, value_local)
      == Some(true)
  );
  assert!(!tc_scope.has_caught());

  // Since TLA is active we need to return a promise.
  let resolver = v8::PromiseResolver::new(tc_scope).unwrap();
  let undefined = v8::undefined(tc_scope);
  resolver.resolve(tc_scope, undefined.into());
  Some(resolver.get_promise(tc_scope).into())
}

/// A type of module to be executed.
///
/// For non-`JavaScript` modules, this value doesn't tell
/// how to interpret the module; it is only used to validate
/// the module against an import assertion (if one is present
/// in the import statement).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[repr(u32)]
pub enum ModuleType {
  JavaScript,
  Json,
}

impl std::fmt::Display for ModuleType {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    match self {
      Self::JavaScript => write!(f, "JavaScript"),
      Self::Json => write!(f, "JSON"),
    }
  }
}

/// EsModule source code that will be loaded into V8.
///
/// Users can implement `Into<ModuleInfo>` for different file types that
/// can be transpiled to valid EsModule.
///
/// Found module URL might be different from specified URL
/// used for loading due to redirections (like HTTP 303).
/// Eg. Both "`https://example.com/a.ts`" and
/// "`https://example.com/b.ts`" may point to "`https://example.com/c.ts`"
/// By keeping track of specified and found URL we can alias modules and avoid
/// recompiling the same code 3 times.
// TODO(bartlomieju): I have a strong opinion we should store all redirects
// that happened; not only first and final target. It would simplify a lot
// of things throughout the codebase otherwise we may end up requesting
// intermediate redirects from file loader.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ModuleSource {
  pub code: Box<[u8]>,
  pub module_type: ModuleType,
  pub module_url_specified: String,
  pub module_url_found: String,
}

pub(crate) type PrepareLoadFuture =
  dyn Future<Output = (ModuleLoadId, Result<RecursiveModuleLoad, Error>)>;
pub type ModuleSourceFuture = dyn Future<Output = Result<ModuleSource, Error>>;

type ModuleLoadFuture =
  dyn Future<Output = Result<(ModuleRequest, ModuleSource), Error>>;

#[derive(Debug, PartialEq, Eq)]
pub enum ResolutionKind {
  /// This kind is used in only one situation: when a module is loaded via
  /// `JsRuntime::load_main_module` and is the top-level module, ie. the one
  /// passed as an argument to `JsRuntime::load_main_module`.
  MainModule,
  /// This kind is returned for all other modules during module load, that are
  /// static imports.
  Import,
  /// This kind is returned for all modules that are loaded as a result of a
  /// call to `import()` API (ie. top-level module as well as all its
  /// dependencies, and any other `import()` calls from that load).
  DynamicImport,
}

pub trait ModuleLoader {
  /// Returns an absolute URL.
  /// When implementing an spec-complaint VM, this should be exactly the
  /// algorithm described here:
  /// <https://html.spec.whatwg.org/multipage/webappapis.html#resolve-a-module-specifier>
  ///
  /// `is_main` can be used to resolve from current working directory or
  /// apply import map for child imports.
  ///
  /// `is_dyn_import` can be used to check permissions or deny
  /// dynamic imports altogether.
  fn resolve(
    &self,
    specifier: &str,
    referrer: &str,
    kind: ResolutionKind,
  ) -> Result<ModuleSpecifier, Error>;

  /// Given ModuleSpecifier, load its source code.
  ///
  /// `is_dyn_import` can be used to check permissions or deny
  /// dynamic imports altogether.
  fn load(
    &self,
    module_specifier: &ModuleSpecifier,
    maybe_referrer: Option<ModuleSpecifier>,
    is_dyn_import: bool,
  ) -> Pin<Box<ModuleSourceFuture>>;

  /// This hook can be used by implementors to do some preparation
  /// work before starting loading of modules.
  ///
  /// For example implementor might download multiple modules in
  /// parallel and transpile them to final JS sources before
  /// yielding control back to the runtime.
  ///
  /// It's not required to implement this method.
  fn prepare_load(
    &self,
    _op_state: Rc<RefCell<OpState>>,
    _module_specifier: &ModuleSpecifier,
    _maybe_referrer: Option<String>,
    _is_dyn_import: bool,
  ) -> Pin<Box<dyn Future<Output = Result<(), Error>>>> {
    async { Ok(()) }.boxed_local()
  }
}

/// Placeholder structure used when creating
/// a runtime that doesn't support module loading.
pub struct NoopModuleLoader;

impl ModuleLoader for NoopModuleLoader {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &str,
    _kind: ResolutionKind,
  ) -> Result<ModuleSpecifier, Error> {
    Err(generic_error(
      format!("Module loading is not supported; attempted to resolve: \"{specifier}\" from \"{referrer}\"")
    ))
  }

  fn load(
    &self,
    module_specifier: &ModuleSpecifier,
    maybe_referrer: Option<ModuleSpecifier>,
    _is_dyn_import: bool,
  ) -> Pin<Box<ModuleSourceFuture>> {
    let err = generic_error(
      format!(
        "Module loading is not supported; attempted to load: \"{module_specifier}\" from \"{maybe_referrer:?}\"",
      )
    );
    async move { Err(err) }.boxed_local()
  }
}

/// Helper function, that calls into `loader.resolve()`, but denies resolution
/// of `internal` scheme if we are running with a snapshot loaded and not
/// creating a snapshot
pub(crate) fn resolve_helper(
  snapshot_loaded_and_not_snapshotting: bool,
  loader: Rc<dyn ModuleLoader>,
  specifier: &str,
  referrer: &str,
  kind: ResolutionKind,
) -> Result<ModuleSpecifier, Error> {
  if snapshot_loaded_and_not_snapshotting && specifier.starts_with("internal:")
  {
    return Err(generic_error(
      "Cannot load internal module from external code",
    ));
  }

  loader.resolve(specifier, referrer, kind)
}

/// Function that can be passed to the `InternalModuleLoader` that allows to
/// transpile sources before passing to V8.
pub type InternalModuleLoaderCb =
  Box<dyn Fn(&ExtensionFileSource) -> Result<String, Error>>;

pub struct InternalModuleLoader {
  module_loader: Rc<dyn ModuleLoader>,
  esm_sources: Vec<ExtensionFileSource>,
  maybe_load_callback: Option<InternalModuleLoaderCb>,
}

impl Default for InternalModuleLoader {
  fn default() -> Self {
    Self {
      module_loader: Rc::new(NoopModuleLoader),
      esm_sources: vec![],
      maybe_load_callback: None,
    }
  }
}

impl InternalModuleLoader {
  pub fn new(
    module_loader: Option<Rc<dyn ModuleLoader>>,
    esm_sources: Vec<ExtensionFileSource>,
    maybe_load_callback: Option<InternalModuleLoaderCb>,
  ) -> Self {
    InternalModuleLoader {
      module_loader: module_loader.unwrap_or_else(|| Rc::new(NoopModuleLoader)),
      esm_sources,
      maybe_load_callback,
    }
  }
}

impl ModuleLoader for InternalModuleLoader {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &str,
    kind: ResolutionKind,
  ) -> Result<ModuleSpecifier, Error> {
    if let Ok(url_specifier) = ModuleSpecifier::parse(specifier) {
      if url_specifier.scheme() == "internal" {
        let referrer_specifier = ModuleSpecifier::parse(referrer).ok();
        if referrer == "." || referrer_specifier.unwrap().scheme() == "internal"
        {
          return Ok(url_specifier);
        } else {
          return Err(generic_error(
            "Cannot load internal module from external code",
          ));
        };
      }
    }

    self.module_loader.resolve(specifier, referrer, kind)
  }

  fn load(
    &self,
    module_specifier: &ModuleSpecifier,
    maybe_referrer: Option<ModuleSpecifier>,
    is_dyn_import: bool,
  ) -> Pin<Box<ModuleSourceFuture>> {
    if module_specifier.scheme() != "internal" {
      return self.module_loader.load(
        module_specifier,
        maybe_referrer,
        is_dyn_import,
      );
    }

    let specifier = module_specifier.to_string();
    let maybe_file_source = self
      .esm_sources
      .iter()
      .find(|file_source| file_source.specifier == module_specifier.as_str());

    if let Some(file_source) = maybe_file_source {
      let result = if let Some(load_callback) = &self.maybe_load_callback {
        load_callback(file_source)
      } else {
        match file_source.code.load() {
          Ok(code) => Ok(code),
          Err(err) => return futures::future::err(err).boxed_local(),
        }
      };

      return async move {
        let code = result?;
        let source = ModuleSource {
          code: code.into_bytes().into_boxed_slice(),
          module_type: ModuleType::JavaScript,
          module_url_specified: specifier.clone(),
          module_url_found: specifier.clone(),
        };
        Ok(source)
      }
      .boxed_local();
    }

    async move {
      Err(generic_error(format!(
        "Cannot find internal module source for specifier {specifier}"
      )))
    }
    .boxed_local()
  }

  fn prepare_load(
    &self,
    op_state: Rc<RefCell<OpState>>,
    module_specifier: &ModuleSpecifier,
    maybe_referrer: Option<String>,
    is_dyn_import: bool,
  ) -> Pin<Box<dyn Future<Output = Result<(), Error>>>> {
    if module_specifier.scheme() == "internal" {
      return async { Ok(()) }.boxed_local();
    }

    self.module_loader.prepare_load(
      op_state,
      module_specifier,
      maybe_referrer,
      is_dyn_import,
    )
  }
}

/// Basic file system module loader.
///
/// Note that this loader will **block** event loop
/// when loading file as it uses synchronous FS API
/// from standard library.
pub struct FsModuleLoader;

impl ModuleLoader for FsModuleLoader {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &str,
    _kind: ResolutionKind,
  ) -> Result<ModuleSpecifier, Error> {
    Ok(resolve_import(specifier, referrer)?)
  }

  fn load(
    &self,
    module_specifier: &ModuleSpecifier,
    _maybe_referrer: Option<ModuleSpecifier>,
    _is_dynamic: bool,
  ) -> Pin<Box<ModuleSourceFuture>> {
    let module_specifier = module_specifier.clone();
    async move {
      let path = module_specifier.to_file_path().map_err(|_| {
        generic_error(format!(
          "Provided module specifier \"{module_specifier}\" is not a file URL."
        ))
      })?;
      let module_type = if let Some(extension) = path.extension() {
        let ext = extension.to_string_lossy().to_lowercase();
        if ext == "json" {
          ModuleType::Json
        } else {
          ModuleType::JavaScript
        }
      } else {
        ModuleType::JavaScript
      };

      let code = std::fs::read(path)?;
      let module = ModuleSource {
        code: code.into_boxed_slice(),
        module_type,
        module_url_specified: module_specifier.to_string(),
        module_url_found: module_specifier.to_string(),
      };
      Ok(module)
    }
    .boxed_local()
  }
}

/// Describes the entrypoint of a recursive module load.
#[derive(Debug)]
enum LoadInit {
  /// Main module specifier.
  Main(String),
  /// Module specifier for side module.
  Side(String),
  /// Dynamic import specifier with referrer and expected
  /// module type (which is determined by import assertion).
  DynamicImport(String, String, AssertedModuleType),
}

#[derive(Debug, Eq, PartialEq)]
pub enum LoadState {
  Init,
  LoadingRoot,
  LoadingImports,
  Done,
}

/// This future is used to implement parallel async module loading.
pub(crate) struct RecursiveModuleLoad {
  pub id: ModuleLoadId,
  pub root_module_id: Option<ModuleId>,
  init: LoadInit,
  root_asserted_module_type: Option<AssertedModuleType>,
  root_module_type: Option<ModuleType>,
  state: LoadState,
  module_map_rc: Rc<RefCell<ModuleMap>>,
  pending: FuturesUnordered<Pin<Box<ModuleLoadFuture>>>,
  visited: HashSet<ModuleRequest>,
  // These three fields are copied from `module_map_rc`, but they are cloned
  // ahead of time to avoid already-borrowed errors.
  op_state: Rc<RefCell<OpState>>,
  loader: Rc<dyn ModuleLoader>,
  snapshot_loaded_and_not_snapshotting: bool,
}

impl RecursiveModuleLoad {
  /// Starts a new asynchronous load of the module graph for given specifier.
  ///
  /// The module corresponding for the given `specifier` will be marked as
  // "the main module" (`import.meta.main` will return `true` for this module).
  fn main(specifier: &str, module_map_rc: Rc<RefCell<ModuleMap>>) -> Self {
    Self::new(LoadInit::Main(specifier.to_string()), module_map_rc)
  }

  /// Starts a new asynchronous load of the module graph for given specifier.
  fn side(specifier: &str, module_map_rc: Rc<RefCell<ModuleMap>>) -> Self {
    Self::new(LoadInit::Side(specifier.to_string()), module_map_rc)
  }

  /// Starts a new asynchronous load of the module graph for given specifier
  /// that was imported using `import()`.
  fn dynamic_import(
    specifier: &str,
    referrer: &str,
    asserted_module_type: AssertedModuleType,
    module_map_rc: Rc<RefCell<ModuleMap>>,
  ) -> Self {
    Self::new(
      LoadInit::DynamicImport(
        specifier.to_string(),
        referrer.to_string(),
        asserted_module_type,
      ),
      module_map_rc,
    )
  }

  fn new(init: LoadInit, module_map_rc: Rc<RefCell<ModuleMap>>) -> Self {
    let id = {
      let mut module_map = module_map_rc.borrow_mut();
      let id = module_map.next_load_id;
      module_map.next_load_id += 1;
      id
    };
    let op_state = module_map_rc.borrow().op_state.clone();
    let loader = module_map_rc.borrow().loader.clone();
    let asserted_module_type = match init {
      LoadInit::DynamicImport(_, _, module_type) => module_type,
      _ => AssertedModuleType::JavaScriptOrWasm,
    };
    let mut load = Self {
      id,
      root_module_id: None,
      root_asserted_module_type: None,
      root_module_type: None,
      init,
      state: LoadState::Init,
      module_map_rc: module_map_rc.clone(),
      snapshot_loaded_and_not_snapshotting: module_map_rc
        .borrow()
        .snapshot_loaded_and_not_snapshotting,
      op_state,
      loader,
      pending: FuturesUnordered::new(),
      visited: HashSet::new(),
    };
    // FIXME(bartlomieju): this seems fishy
    // Ignore the error here, let it be hit in `Stream::poll_next()`.
    if let Ok(root_specifier) = load.resolve_root() {
      if let Some(module_id) = module_map_rc
        .borrow()
        .get_id(root_specifier.as_str(), asserted_module_type)
      {
        load.root_module_id = Some(module_id);
        load.root_asserted_module_type = Some(asserted_module_type);
        load.root_module_type = Some(
          module_map_rc
            .borrow()
            .get_info_by_id(module_id)
            .unwrap()
            .module_type,
        );
      }
    }
    load
  }

  fn resolve_root(&self) -> Result<ModuleSpecifier, Error> {
    match self.init {
      LoadInit::Main(ref specifier) => resolve_helper(
        self.snapshot_loaded_and_not_snapshotting,
        self.loader.clone(),
        specifier,
        ".",
        ResolutionKind::MainModule,
      ),
      LoadInit::Side(ref specifier) => resolve_helper(
        self.snapshot_loaded_and_not_snapshotting,
        self.loader.clone(),
        specifier,
        ".",
        ResolutionKind::Import,
      ),
      LoadInit::DynamicImport(ref specifier, ref referrer, _) => {
        resolve_helper(
          self.snapshot_loaded_and_not_snapshotting,
          self.loader.clone(),
          specifier,
          referrer,
          ResolutionKind::DynamicImport,
        )
      }
    }
  }

  async fn prepare(&self) -> Result<(), Error> {
    let op_state = self.op_state.clone();

    let (module_specifier, maybe_referrer) = match self.init {
      LoadInit::Main(ref specifier) => {
        let spec = resolve_helper(
          self.snapshot_loaded_and_not_snapshotting,
          self.loader.clone(),
          specifier,
          ".",
          ResolutionKind::MainModule,
        )?;
        (spec, None)
      }
      LoadInit::Side(ref specifier) => {
        let spec = resolve_helper(
          self.snapshot_loaded_and_not_snapshotting,
          self.loader.clone(),
          specifier,
          ".",
          ResolutionKind::Import,
        )?;
        (spec, None)
      }
      LoadInit::DynamicImport(ref specifier, ref referrer, _) => {
        let spec = resolve_helper(
          self.snapshot_loaded_and_not_snapshotting,
          self.loader.clone(),
          specifier,
          referrer,
          ResolutionKind::DynamicImport,
        )?;
        (spec, Some(referrer.to_string()))
      }
    };

    self
      .loader
      .prepare_load(
        op_state,
        &module_specifier,
        maybe_referrer,
        self.is_dynamic_import(),
      )
      .await
  }

  fn is_currently_loading_main_module(&self) -> bool {
    !self.is_dynamic_import()
      && matches!(self.init, LoadInit::Main(..))
      && self.state == LoadState::LoadingRoot
  }

  fn is_dynamic_import(&self) -> bool {
    matches!(self.init, LoadInit::DynamicImport(..))
  }

  pub(crate) fn register_and_recurse(
    &mut self,
    scope: &mut v8::HandleScope,
    module_request: &ModuleRequest,
    module_source: &ModuleSource,
  ) -> Result<(), ModuleError> {
    let expected_asserted_module_type = module_source.module_type.into();
    if module_request.asserted_module_type != expected_asserted_module_type {
      return Err(ModuleError::Other(generic_error(format!(
        "Expected a \"{}\" module but loaded a \"{}\" module.",
        module_request.asserted_module_type, module_source.module_type,
      ))));
    }

    // Register the module in the module map unless it's already there. If the
    // specified URL and the "true" URL are different, register the alias.
    if module_source.module_url_specified != module_source.module_url_found {
      self.module_map_rc.borrow_mut().alias(
        &module_source.module_url_specified,
        expected_asserted_module_type,
        &module_source.module_url_found,
      );
    }
    let maybe_module_id = self.module_map_rc.borrow().get_id(
      &module_source.module_url_found,
      expected_asserted_module_type,
    );
    let module_id = match maybe_module_id {
      Some(id) => {
        debug!(
          "Already-registered module fetched again: {}",
          module_source.module_url_found
        );
        id
      }
      None => match module_source.module_type {
        ModuleType::JavaScript => {
          self.module_map_rc.borrow_mut().new_es_module(
            scope,
            self.is_currently_loading_main_module(),
            &module_source.module_url_found,
            &module_source.code,
            self.is_dynamic_import(),
          )?
        }
        ModuleType::Json => self.module_map_rc.borrow_mut().new_json_module(
          scope,
          &module_source.module_url_found,
          &module_source.code,
        )?,
      },
    };

    // Recurse the module's imports. There are two cases for each import:
    // 1. If the module is not in the module map, start a new load for it in
    //    `self.pending`. The result of that load should eventually be passed to
    //    this function for recursion.
    // 2. If the module is already in the module map, queue it up to be
    //    recursed synchronously here.
    // This robustly ensures that the whole graph is in the module map before
    // `LoadState::Done` is set.
    let mut already_registered = VecDeque::new();
    already_registered.push_back((module_id, module_request.clone()));
    self.visited.insert(module_request.clone());
    while let Some((module_id, module_request)) = already_registered.pop_front()
    {
      let referrer = ModuleSpecifier::parse(&module_request.specifier).unwrap();
      let imports = self
        .module_map_rc
        .borrow()
        .get_requested_modules(module_id)
        .unwrap()
        .clone();
      for module_request in imports {
        if !self.visited.contains(&module_request) {
          if let Some(module_id) = self.module_map_rc.borrow().get_id(
            module_request.specifier.as_str(),
            module_request.asserted_module_type,
          ) {
            already_registered.push_back((module_id, module_request.clone()));
          } else {
            let request = module_request.clone();
            let specifier =
              ModuleSpecifier::parse(&module_request.specifier).unwrap();
            let referrer = referrer.clone();
            let loader = self.loader.clone();
            let is_dynamic_import = self.is_dynamic_import();
            let fut = async move {
              let load_result = loader
                .load(&specifier, Some(referrer.clone()), is_dynamic_import)
                .await;
              load_result.map(|s| (request, s))
            };
            self.pending.push(fut.boxed_local());
          }
          self.visited.insert(module_request);
        }
      }
    }

    // Update `self.state` however applicable.
    if self.state == LoadState::LoadingRoot {
      self.root_module_id = Some(module_id);
      self.root_asserted_module_type = Some(module_source.module_type.into());
      self.state = LoadState::LoadingImports;
    }
    if self.pending.is_empty() {
      self.state = LoadState::Done;
    }

    Ok(())
  }
}

impl Stream for RecursiveModuleLoad {
  type Item = Result<(ModuleRequest, ModuleSource), Error>;

  fn poll_next(
    self: Pin<&mut Self>,
    cx: &mut Context,
  ) -> Poll<Option<Self::Item>> {
    let inner = self.get_mut();
    // IMPORTANT: Do not borrow `inner.module_map_rc` here. It may not be
    // available.
    match inner.state {
      LoadState::Init => {
        let module_specifier = match inner.resolve_root() {
          Ok(url) => url,
          Err(error) => return Poll::Ready(Some(Err(error))),
        };
        let load_fut = if let Some(_module_id) = inner.root_module_id {
          // FIXME(bartlomieju): this is very bad
          // The root module is already in the module map.
          // TODO(nayeemrmn): In this case we would ideally skip to
          // `LoadState::LoadingImports` and synchronously recurse the imports
          // like the bottom of `RecursiveModuleLoad::register_and_recurse()`.
          // But the module map cannot be borrowed here. Instead fake a load
          // event so it gets passed to that function and recursed eventually.
          let asserted_module_type = inner.root_asserted_module_type.unwrap();
          let module_type = inner.root_module_type.unwrap();
          let module_request = ModuleRequest {
            specifier: module_specifier.to_string(),
            asserted_module_type,
          };
          let module_source = ModuleSource {
            module_url_specified: module_specifier.to_string(),
            module_url_found: module_specifier.to_string(),
            // The code will be discarded, since this module is already in the
            // module map.
            code: Default::default(),
            module_type,
          };
          futures::future::ok((module_request, module_source)).boxed()
        } else {
          let maybe_referrer = match inner.init {
            LoadInit::DynamicImport(_, ref referrer, _) => {
              resolve_url(referrer).ok()
            }
            _ => None,
          };
          let asserted_module_type = match inner.init {
            LoadInit::DynamicImport(_, _, module_type) => module_type,
            _ => AssertedModuleType::JavaScriptOrWasm,
          };
          let module_request = ModuleRequest {
            specifier: module_specifier.to_string(),
            asserted_module_type,
          };
          let loader = inner.loader.clone();
          let is_dynamic_import = inner.is_dynamic_import();
          async move {
            let result = loader
              .load(&module_specifier, maybe_referrer, is_dynamic_import)
              .await;
            result.map(|s| (module_request, s))
          }
          .boxed_local()
        };
        inner.pending.push(load_fut);
        inner.state = LoadState::LoadingRoot;
        inner.try_poll_next_unpin(cx)
      }
      LoadState::LoadingRoot | LoadState::LoadingImports => {
        match inner.pending.try_poll_next_unpin(cx)? {
          Poll::Ready(None) => unreachable!(),
          Poll::Ready(Some(info)) => Poll::Ready(Some(Ok(info))),
          Poll::Pending => Poll::Pending,
        }
      }
      LoadState::Done => Poll::Ready(None),
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[repr(u32)]
pub(crate) enum AssertedModuleType {
  JavaScriptOrWasm,
  Json,
}

impl From<ModuleType> for AssertedModuleType {
  fn from(module_type: ModuleType) -> AssertedModuleType {
    match module_type {
      ModuleType::JavaScript => AssertedModuleType::JavaScriptOrWasm,
      ModuleType::Json => AssertedModuleType::Json,
    }
  }
}

impl std::fmt::Display for AssertedModuleType {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    match self {
      Self::JavaScriptOrWasm => write!(f, "JavaScriptOrWasm"),
      Self::Json => write!(f, "JSON"),
    }
  }
}

/// Describes a request for a module as parsed from the source code.
/// Usually executable (`JavaScriptOrWasm`) is used, except when an
/// import assertions explicitly constrains an import to JSON, in
/// which case this will have a `AssertedModuleType::Json`.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub(crate) struct ModuleRequest {
  pub specifier: String,
  pub asserted_module_type: AssertedModuleType,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ModuleInfo {
  #[allow(unused)]
  pub id: ModuleId,
  // Used in "bindings.rs" for "import.meta.main" property value.
  pub main: bool,
  pub name: String,
  pub requests: Vec<ModuleRequest>,
  pub module_type: ModuleType,
}

/// A symbolic module entity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) enum SymbolicModule {
  /// This module is an alias to another module.
  /// This is useful such that multiple names could point to
  /// the same underlying module (particularly due to redirects).
  Alias(String),
  /// This module associates with a V8 module by id.
  Mod(ModuleId),
}

#[derive(Debug)]
pub(crate) enum ModuleError {
  Exception(v8::Global<v8::Value>),
  Other(Error),
}

/// A collection of JS modules.
pub(crate) struct ModuleMap {
  // Handling of specifiers and v8 objects
  pub handles: Vec<v8::Global<v8::Module>>,
  pub info: Vec<ModuleInfo>,
  pub(crate) by_name: HashMap<(String, AssertedModuleType), SymbolicModule>,
  pub(crate) next_load_id: ModuleLoadId,

  // Handling of futures for loading module sources
  pub loader: Rc<dyn ModuleLoader>,
  op_state: Rc<RefCell<OpState>>,
  pub(crate) dynamic_import_map:
    HashMap<ModuleLoadId, v8::Global<v8::PromiseResolver>>,
  pub(crate) preparing_dynamic_imports:
    FuturesUnordered<Pin<Box<PrepareLoadFuture>>>,
  pub(crate) pending_dynamic_imports:
    FuturesUnordered<StreamFuture<RecursiveModuleLoad>>,

  // This store is used temporarly, to forward parsed JSON
  // value from `new_json_module` to `json_module_evaluation_steps`
  json_value_store: HashMap<v8::Global<v8::Module>, v8::Global<v8::Value>>,

  pub(crate) snapshot_loaded_and_not_snapshotting: bool,
}

impl ModuleMap {
  pub fn serialize_for_snapshotting(
    &self,
    scope: &mut v8::HandleScope,
  ) -> (v8::Global<v8::Array>, Vec<v8::Global<v8::Module>>) {
    let array = v8::Array::new(scope, 3);

    let next_load_id = v8::Integer::new(scope, self.next_load_id);
    array.set_index(scope, 0, next_load_id.into());

    let info_arr = v8::Array::new(scope, self.info.len() as i32);
    for (i, info) in self.info.iter().enumerate() {
      let module_info_arr = v8::Array::new(scope, 5);

      let id = v8::Integer::new(scope, info.id as i32);
      module_info_arr.set_index(scope, 0, id.into());

      let main = v8::Boolean::new(scope, info.main);
      module_info_arr.set_index(scope, 1, main.into());

      let name = v8::String::new(scope, &info.name).unwrap();
      module_info_arr.set_index(scope, 2, name.into());

      let array_len = 2 * info.requests.len() as i32;
      let requests_arr = v8::Array::new(scope, array_len);
      for (i, request) in info.requests.iter().enumerate() {
        let specifier = v8::String::new(scope, &request.specifier).unwrap();
        requests_arr.set_index(scope, 2 * i as u32, specifier.into());

        let asserted_module_type =
          v8::Integer::new(scope, request.asserted_module_type as i32);
        requests_arr.set_index(
          scope,
          (2 * i) as u32 + 1,
          asserted_module_type.into(),
        );
      }
      module_info_arr.set_index(scope, 3, requests_arr.into());

      let module_type = v8::Integer::new(scope, info.module_type as i32);
      module_info_arr.set_index(scope, 4, module_type.into());

      info_arr.set_index(scope, i as u32, module_info_arr.into());
    }
    array.set_index(scope, 1, info_arr.into());

    let by_name_array = v8::Array::new(scope, self.by_name.len() as i32);
    {
      for (i, elem) in self.by_name.iter().enumerate() {
        let arr = v8::Array::new(scope, 3);

        let (specifier, asserted_module_type) = elem.0;
        let specifier = v8::String::new(scope, specifier).unwrap();
        arr.set_index(scope, 0, specifier.into());

        let asserted_module_type =
          v8::Integer::new(scope, *asserted_module_type as i32);
        arr.set_index(scope, 1, asserted_module_type.into());

        let symbolic_module: v8::Local<v8::Value> = match &elem.1 {
          SymbolicModule::Alias(alias) => {
            let alias = v8::String::new(scope, alias).unwrap();
            alias.into()
          }
          SymbolicModule::Mod(id) => {
            let id = v8::Integer::new(scope, *id as i32);
            id.into()
          }
        };
        arr.set_index(scope, 2, symbolic_module);

        by_name_array.set_index(scope, i as u32, arr.into());
      }
    }
    array.set_index(scope, 2, by_name_array.into());

    let array_global = v8::Global::new(scope, array);

    let handles = self.handles.clone();
    (array_global, handles)
  }

  pub fn update_with_snapshot_data(
    &mut self,
    scope: &mut v8::HandleScope,
    data: v8::Global<v8::Array>,
    module_handles: Vec<v8::Global<v8::Module>>,
  ) {
    let local_data: v8::Local<v8::Array> = v8::Local::new(scope, data);

    {
      let next_load_id = local_data.get_index(scope, 0).unwrap();
      assert!(next_load_id.is_int32());
      let integer = next_load_id.to_integer(scope).unwrap();
      let val = integer.int32_value(scope).unwrap();
      self.next_load_id = val;
    }

    {
      let info_val = local_data.get_index(scope, 1).unwrap();

      let info_arr: v8::Local<v8::Array> = info_val.try_into().unwrap();
      let len = info_arr.length() as usize;
      let mut info = Vec::with_capacity(len);

      for i in 0..len {
        let module_info_arr: v8::Local<v8::Array> = info_arr
          .get_index(scope, i as u32)
          .unwrap()
          .try_into()
          .unwrap();
        let id = module_info_arr
          .get_index(scope, 0)
          .unwrap()
          .to_integer(scope)
          .unwrap()
          .value() as ModuleId;

        let main = module_info_arr
          .get_index(scope, 1)
          .unwrap()
          .to_boolean(scope)
          .is_true();

        let name = module_info_arr
          .get_index(scope, 2)
          .unwrap()
          .to_rust_string_lossy(scope);

        let requests_arr: v8::Local<v8::Array> = module_info_arr
          .get_index(scope, 3)
          .unwrap()
          .try_into()
          .unwrap();
        let len = (requests_arr.length() as usize) / 2;
        let mut requests = Vec::with_capacity(len);
        for i in 0..len {
          let specifier = requests_arr
            .get_index(scope, (2 * i) as u32)
            .unwrap()
            .to_rust_string_lossy(scope);
          let asserted_module_type_no = requests_arr
            .get_index(scope, (2 * i + 1) as u32)
            .unwrap()
            .to_integer(scope)
            .unwrap()
            .value();
          let asserted_module_type = match asserted_module_type_no {
            0 => AssertedModuleType::JavaScriptOrWasm,
            1 => AssertedModuleType::Json,
            _ => unreachable!(),
          };
          requests.push(ModuleRequest {
            specifier,
            asserted_module_type,
          });
        }

        let module_type_no = module_info_arr
          .get_index(scope, 4)
          .unwrap()
          .to_integer(scope)
          .unwrap()
          .value();
        let module_type = match module_type_no {
          0 => ModuleType::JavaScript,
          1 => ModuleType::Json,
          _ => unreachable!(),
        };

        let module_info = ModuleInfo {
          id,
          main,
          name,
          requests,
          module_type,
        };
        info.push(module_info);
      }

      self.info = info;
    }

    {
      let by_name_arr: v8::Local<v8::Array> =
        local_data.get_index(scope, 2).unwrap().try_into().unwrap();
      let len = by_name_arr.length() as usize;
      let mut by_name = HashMap::with_capacity(len);

      for i in 0..len {
        let arr: v8::Local<v8::Array> = by_name_arr
          .get_index(scope, i as u32)
          .unwrap()
          .try_into()
          .unwrap();

        let specifier =
          arr.get_index(scope, 0).unwrap().to_rust_string_lossy(scope);
        let asserted_module_type = match arr
          .get_index(scope, 1)
          .unwrap()
          .to_integer(scope)
          .unwrap()
          .value()
        {
          0 => AssertedModuleType::JavaScriptOrWasm,
          1 => AssertedModuleType::Json,
          _ => unreachable!(),
        };
        let key = (specifier, asserted_module_type);

        let symbolic_module_val = arr.get_index(scope, 2).unwrap();
        let val = if symbolic_module_val.is_number() {
          SymbolicModule::Mod(
            symbolic_module_val
              .to_integer(scope)
              .unwrap()
              .value()
              .try_into()
              .unwrap(),
          )
        } else {
          SymbolicModule::Alias(symbolic_module_val.to_rust_string_lossy(scope))
        };

        by_name.insert(key, val);
      }

      self.by_name = by_name;
    }

    self.handles = module_handles;
  }

  pub(crate) fn new(
    loader: Rc<dyn ModuleLoader>,
    op_state: Rc<RefCell<OpState>>,
    snapshot_loaded_and_not_snapshotting: bool,
  ) -> ModuleMap {
    Self {
      handles: vec![],
      info: vec![],
      by_name: HashMap::new(),
      next_load_id: 1,
      loader,
      op_state,
      dynamic_import_map: HashMap::new(),
      preparing_dynamic_imports: FuturesUnordered::new(),
      pending_dynamic_imports: FuturesUnordered::new(),
      json_value_store: HashMap::new(),
      snapshot_loaded_and_not_snapshotting,
    }
  }

  /// Get module id, following all aliases in case of module specifier
  /// that had been redirected.
  fn get_id(
    &self,
    name: &str,
    asserted_module_type: AssertedModuleType,
  ) -> Option<ModuleId> {
    let mut mod_name = name;
    loop {
      let symbolic_module = self
        .by_name
        .get(&(mod_name.to_string(), asserted_module_type))?;
      match symbolic_module {
        SymbolicModule::Alias(target) => {
          mod_name = target;
        }
        SymbolicModule::Mod(mod_id) => return Some(*mod_id),
      }
    }
  }

  fn new_json_module(
    &mut self,
    scope: &mut v8::HandleScope,
    name: &str,
    source: &[u8],
  ) -> Result<ModuleId, ModuleError> {
    let name_str = v8::String::new(scope, name).unwrap();
    let source_str = v8::String::new_from_utf8(
      scope,
      strip_bom(source),
      v8::NewStringType::Normal,
    )
    .unwrap();

    let tc_scope = &mut v8::TryCatch::new(scope);

    let parsed_json = match v8::json::parse(tc_scope, source_str) {
      Some(parsed_json) => parsed_json,
      None => {
        assert!(tc_scope.has_caught());
        let exception = tc_scope.exception().unwrap();
        let exception = v8::Global::new(tc_scope, exception);
        return Err(ModuleError::Exception(exception));
      }
    };

    let export_names = [v8::String::new(tc_scope, "default").unwrap()];
    let module = v8::Module::create_synthetic_module(
      tc_scope,
      name_str,
      &export_names,
      json_module_evaluation_steps,
    );

    let handle = v8::Global::<v8::Module>::new(tc_scope, module);
    let value_handle = v8::Global::<v8::Value>::new(tc_scope, parsed_json);
    self.json_value_store.insert(handle.clone(), value_handle);

    let id =
      self.create_module_info(name, ModuleType::Json, handle, false, vec![]);

    Ok(id)
  }

  // Create and compile an ES module.
  pub(crate) fn new_es_module(
    &mut self,
    scope: &mut v8::HandleScope,
    main: bool,
    name: &str,
    source: &[u8],
    is_dynamic_import: bool,
  ) -> Result<ModuleId, ModuleError> {
    let name_str = v8::String::new(scope, name).unwrap();
    let source_str =
      v8::String::new_from_utf8(scope, source, v8::NewStringType::Normal)
        .unwrap();

    let origin = bindings::module_origin(scope, name_str);
    let source = v8::script_compiler::Source::new(source_str, Some(&origin));

    let tc_scope = &mut v8::TryCatch::new(scope);

    let maybe_module = v8::script_compiler::compile_module(tc_scope, source);

    if tc_scope.has_caught() {
      assert!(maybe_module.is_none());
      let exception = tc_scope.exception().unwrap();
      let exception = v8::Global::new(tc_scope, exception);
      return Err(ModuleError::Exception(exception));
    }

    let module = maybe_module.unwrap();

    let mut requests: Vec<ModuleRequest> = vec![];
    let module_requests = module.get_module_requests();
    for i in 0..module_requests.length() {
      let module_request = v8::Local::<v8::ModuleRequest>::try_from(
        module_requests.get(tc_scope, i).unwrap(),
      )
      .unwrap();
      let import_specifier = module_request
        .get_specifier()
        .to_rust_string_lossy(tc_scope);

      let import_assertions = module_request.get_import_assertions();

      let assertions = parse_import_assertions(
        tc_scope,
        import_assertions,
        ImportAssertionsKind::StaticImport,
      );

      // FIXME(bartomieju): there are no stack frames if exception
      // is thrown here
      validate_import_assertions(tc_scope, &assertions);
      if tc_scope.has_caught() {
        let exception = tc_scope.exception().unwrap();
        let exception = v8::Global::new(tc_scope, exception);
        return Err(ModuleError::Exception(exception));
      }

      let module_specifier = match resolve_helper(
        self.snapshot_loaded_and_not_snapshotting,
        self.loader.clone(),
        &import_specifier,
        name,
        if is_dynamic_import {
          ResolutionKind::DynamicImport
        } else {
          ResolutionKind::Import
        },
      ) {
        Ok(s) => s,
        Err(e) => return Err(ModuleError::Other(e)),
      };
      let asserted_module_type =
        get_asserted_module_type_from_assertions(&assertions);
      let request = ModuleRequest {
        specifier: module_specifier.to_string(),
        asserted_module_type,
      };
      requests.push(request);
    }

    if main {
      let maybe_main_module = self.info.iter().find(|module| module.main);
      if let Some(main_module) = maybe_main_module {
        return Err(ModuleError::Other(generic_error(
          format!("Trying to create \"main\" module ({:?}), when one already exists ({:?})",
          name,
          main_module.name,
        ))));
      }
    }

    let handle = v8::Global::<v8::Module>::new(tc_scope, module);
    let id = self.create_module_info(
      name,
      ModuleType::JavaScript,
      handle,
      main,
      requests,
    );

    Ok(id)
  }

  fn create_module_info(
    &mut self,
    name: &str,
    module_type: ModuleType,
    handle: v8::Global<v8::Module>,
    main: bool,
    requests: Vec<ModuleRequest>,
  ) -> ModuleId {
    let id = self.handles.len();
    self.by_name.insert(
      (name.to_string(), module_type.into()),
      SymbolicModule::Mod(id),
    );
    self.handles.push(handle);
    self.info.push(ModuleInfo {
      id,
      main,
      name: name.to_string(),
      requests,
      module_type,
    });

    id
  }

  fn get_requested_modules(&self, id: ModuleId) -> Option<&Vec<ModuleRequest>> {
    self.info.get(id).map(|i| &i.requests)
  }

  fn is_registered(
    &self,
    specifier: &ModuleSpecifier,
    asserted_module_type: AssertedModuleType,
  ) -> bool {
    if let Some(id) = self.get_id(specifier.as_str(), asserted_module_type) {
      let info = self.get_info_by_id(id).unwrap();
      return asserted_module_type == info.module_type.into();
    }

    false
  }

  fn alias(
    &mut self,
    name: &str,
    asserted_module_type: AssertedModuleType,
    target: &str,
  ) {
    self.by_name.insert(
      (name.to_string(), asserted_module_type),
      SymbolicModule::Alias(target.to_string()),
    );
  }

  #[cfg(test)]
  fn is_alias(
    &self,
    name: &str,
    asserted_module_type: AssertedModuleType,
  ) -> bool {
    let cond = self.by_name.get(&(name.to_string(), asserted_module_type));
    matches!(cond, Some(SymbolicModule::Alias(_)))
  }

  pub(crate) fn get_handle(
    &self,
    id: ModuleId,
  ) -> Option<v8::Global<v8::Module>> {
    self.handles.get(id).cloned()
  }

  pub(crate) fn get_info(
    &self,
    global: &v8::Global<v8::Module>,
  ) -> Option<&ModuleInfo> {
    if let Some(id) = self.handles.iter().position(|module| module == global) {
      return self.info.get(id);
    }

    None
  }

  pub(crate) fn get_info_by_id(&self, id: ModuleId) -> Option<&ModuleInfo> {
    self.info.get(id)
  }

  pub(crate) async fn load_main(
    module_map_rc: Rc<RefCell<ModuleMap>>,
    specifier: &str,
  ) -> Result<RecursiveModuleLoad, Error> {
    let load = RecursiveModuleLoad::main(specifier, module_map_rc.clone());
    load.prepare().await?;
    Ok(load)
  }

  pub(crate) async fn load_side(
    module_map_rc: Rc<RefCell<ModuleMap>>,
    specifier: &str,
  ) -> Result<RecursiveModuleLoad, Error> {
    let load = RecursiveModuleLoad::side(specifier, module_map_rc.clone());
    load.prepare().await?;
    Ok(load)
  }

  // Initiate loading of a module graph imported using `import()`.
  pub(crate) fn load_dynamic_import(
    module_map_rc: Rc<RefCell<ModuleMap>>,
    specifier: &str,
    referrer: &str,
    asserted_module_type: AssertedModuleType,
    resolver_handle: v8::Global<v8::PromiseResolver>,
  ) {
    let load = RecursiveModuleLoad::dynamic_import(
      specifier,
      referrer,
      asserted_module_type,
      module_map_rc.clone(),
    );
    module_map_rc
      .borrow_mut()
      .dynamic_import_map
      .insert(load.id, resolver_handle);

    let (loader, snapshot_loaded_and_not_snapshotting) = {
      let module_map = module_map_rc.borrow();
      (
        module_map.loader.clone(),
        module_map.snapshot_loaded_and_not_snapshotting,
      )
    };
    let resolve_result = resolve_helper(
      snapshot_loaded_and_not_snapshotting,
      loader,
      specifier,
      referrer,
      ResolutionKind::DynamicImport,
    );
    let fut = match resolve_result {
      Ok(module_specifier) => {
        if module_map_rc
          .borrow()
          .is_registered(&module_specifier, asserted_module_type)
        {
          async move { (load.id, Ok(load)) }.boxed_local()
        } else {
          async move { (load.id, load.prepare().await.map(|()| load)) }
            .boxed_local()
        }
      }
      Err(error) => async move { (load.id, Err(error)) }.boxed_local(),
    };
    module_map_rc
      .borrow_mut()
      .preparing_dynamic_imports
      .push(fut);
  }

  pub(crate) fn has_pending_dynamic_imports(&self) -> bool {
    !(self.preparing_dynamic_imports.is_empty()
      && self.pending_dynamic_imports.is_empty())
  }

  /// Called by `module_resolve_callback` during module instantiation.
  pub(crate) fn resolve_callback<'s>(
    &self,
    scope: &mut v8::HandleScope<'s>,
    specifier: &str,
    referrer: &str,
    import_assertions: HashMap<String, String>,
  ) -> Option<v8::Local<'s, v8::Module>> {
    let resolved_specifier = resolve_helper(
      self.snapshot_loaded_and_not_snapshotting,
      self.loader.clone(),
      specifier,
      referrer,
      ResolutionKind::Import,
    )
    .expect("Module should have been already resolved");

    let module_type =
      get_asserted_module_type_from_assertions(&import_assertions);

    if let Some(id) = self.get_id(resolved_specifier.as_str(), module_type) {
      if let Some(handle) = self.get_handle(id) {
        return Some(v8::Local::new(scope, handle));
      }
    }

    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Extension;
  use crate::JsRuntime;
  use crate::RuntimeOptions;
  use crate::Snapshot;
  use deno_ops::op;
  use futures::future::FutureExt;
  use parking_lot::Mutex;
  use std::fmt;
  use std::future::Future;
  use std::io;
  use std::path::PathBuf;
  use std::sync::atomic::AtomicUsize;
  use std::sync::atomic::Ordering;
  use std::sync::Arc;
  // deno_ops macros generate code assuming deno_core in scope.
  mod deno_core {
    pub use crate::*;
  }

  // TODO(ry) Sadly FuturesUnordered requires the current task to be set. So
  // even though we are only using poll() in these tests and not Tokio, we must
  // nevertheless run it in the tokio executor. Ideally run_in_task can be
  // removed in the future.
  use crate::runtime::tests::run_in_task;

  #[derive(Default)]
  struct MockLoader {
    pub loads: Arc<Mutex<Vec<String>>>,
  }

  impl MockLoader {
    fn new() -> Rc<Self> {
      Default::default()
    }
  }

  fn mock_source_code(url: &str) -> Option<(&'static str, &'static str)> {
    const A_SRC: &str = r#"
import { b } from "/b.js";
import { c } from "/c.js";
if (b() != 'b') throw Error();
if (c() != 'c') throw Error();
if (!import.meta.main) throw Error();
if (import.meta.url != 'file:///a.js') throw Error();
"#;

    const B_SRC: &str = r#"
import { c } from "/c.js";
if (c() != 'c') throw Error();
export function b() { return 'b'; }
if (import.meta.main) throw Error();
if (import.meta.url != 'file:///b.js') throw Error();
"#;

    const C_SRC: &str = r#"
import { d } from "/d.js";
export function c() { return 'c'; }
if (d() != 'd') throw Error();
if (import.meta.main) throw Error();
if (import.meta.url != 'file:///c.js') throw Error();
"#;

    const D_SRC: &str = r#"
export function d() { return 'd'; }
if (import.meta.main) throw Error();
if (import.meta.url != 'file:///d.js') throw Error();
"#;

    const CIRCULAR1_SRC: &str = r#"
import "/circular2.js";
Deno.core.print("circular1");
"#;

    const CIRCULAR2_SRC: &str = r#"
import "/circular3.js";
Deno.core.print("circular2");
"#;

    const CIRCULAR3_SRC: &str = r#"
import "/circular1.js";
import "/circular2.js";
Deno.core.print("circular3");
"#;

    const REDIRECT1_SRC: &str = r#"
import "./redirect2.js";
Deno.core.print("redirect1");
"#;

    const REDIRECT2_SRC: &str = r#"
import "./redirect3.js";
Deno.core.print("redirect2");
"#;

    const REDIRECT3_SRC: &str = r#"Deno.core.print("redirect3");"#;

    const MAIN_SRC: &str = r#"
// never_ready.js never loads.
import "/never_ready.js";
// slow.js resolves after one tick.
import "/slow.js";
"#;

    const SLOW_SRC: &str = r#"
// Circular import of never_ready.js
// Does this trigger two ModuleLoader calls? It shouldn't.
import "/never_ready.js";
import "/a.js";
"#;

    const BAD_IMPORT_SRC: &str = r#"import "foo";"#;

    // (code, real_module_name)
    let spec: Vec<&str> = url.split("file://").collect();
    match spec[1] {
      "/a.js" => Some((A_SRC, "file:///a.js")),
      "/b.js" => Some((B_SRC, "file:///b.js")),
      "/c.js" => Some((C_SRC, "file:///c.js")),
      "/d.js" => Some((D_SRC, "file:///d.js")),
      "/circular1.js" => Some((CIRCULAR1_SRC, "file:///circular1.js")),
      "/circular2.js" => Some((CIRCULAR2_SRC, "file:///circular2.js")),
      "/circular3.js" => Some((CIRCULAR3_SRC, "file:///circular3.js")),
      "/redirect1.js" => Some((REDIRECT1_SRC, "file:///redirect1.js")),
      // pretend redirect - real module name is different than one requested
      "/redirect2.js" => Some((REDIRECT2_SRC, "file:///dir/redirect2.js")),
      "/dir/redirect3.js" => Some((REDIRECT3_SRC, "file:///redirect3.js")),
      "/slow.js" => Some((SLOW_SRC, "file:///slow.js")),
      "/never_ready.js" => {
        Some(("should never be Ready", "file:///never_ready.js"))
      }
      "/main.js" => Some((MAIN_SRC, "file:///main.js")),
      "/bad_import.js" => Some((BAD_IMPORT_SRC, "file:///bad_import.js")),
      // deliberately empty code.
      "/main_with_code.js" => Some(("", "file:///main_with_code.js")),
      _ => None,
    }
  }

  #[derive(Debug, PartialEq)]
  enum MockError {
    ResolveErr,
    LoadErr,
  }

  impl fmt::Display for MockError {
    fn fmt(&self, _f: &mut fmt::Formatter) -> fmt::Result {
      unimplemented!()
    }
  }

  impl std::error::Error for MockError {
    fn cause(&self) -> Option<&dyn std::error::Error> {
      unimplemented!()
    }
  }

  struct DelayedSourceCodeFuture {
    url: String,
    counter: u32,
  }

  impl Future for DelayedSourceCodeFuture {
    type Output = Result<ModuleSource, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
      let inner = self.get_mut();
      inner.counter += 1;
      if inner.url == "file:///never_ready.js" {
        return Poll::Pending;
      }
      if inner.url == "file:///slow.js" && inner.counter < 2 {
        // TODO(ry) Hopefully in the future we can remove current task
        // notification. See comment above run_in_task.
        cx.waker().wake_by_ref();
        return Poll::Pending;
      }
      match mock_source_code(&inner.url) {
        Some(src) => Poll::Ready(Ok(ModuleSource {
          code: src.0.as_bytes().to_vec().into_boxed_slice(),
          module_type: ModuleType::JavaScript,
          module_url_specified: inner.url.clone(),
          module_url_found: src.1.to_owned(),
        })),
        None => Poll::Ready(Err(MockError::LoadErr.into())),
      }
    }
  }

  impl ModuleLoader for MockLoader {
    fn resolve(
      &self,
      specifier: &str,
      referrer: &str,
      _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, Error> {
      let referrer = if referrer == "." {
        "file:///"
      } else {
        referrer
      };

      let output_specifier = match resolve_import(specifier, referrer) {
        Ok(specifier) => specifier,
        Err(..) => return Err(MockError::ResolveErr.into()),
      };

      if mock_source_code(output_specifier.as_ref()).is_some() {
        Ok(output_specifier)
      } else {
        Err(MockError::ResolveErr.into())
      }
    }

    fn load(
      &self,
      module_specifier: &ModuleSpecifier,
      _maybe_referrer: Option<ModuleSpecifier>,
      _is_dyn_import: bool,
    ) -> Pin<Box<ModuleSourceFuture>> {
      let mut loads = self.loads.lock();
      loads.push(module_specifier.to_string());
      let url = module_specifier.to_string();
      DelayedSourceCodeFuture { url, counter: 0 }.boxed()
    }
  }

  #[test]
  fn test_recursive_load() {
    let loader = MockLoader::new();
    let loads = loader.loads.clone();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });
    let spec = resolve_url("file:///a.js").unwrap();
    let a_id_fut = runtime.load_main_module(&spec, None);
    let a_id = futures::executor::block_on(a_id_fut).unwrap();

    #[allow(clippy::let_underscore_future)]
    let _ = runtime.mod_evaluate(a_id);
    futures::executor::block_on(runtime.run_event_loop(false)).unwrap();
    let l = loads.lock();
    assert_eq!(
      l.to_vec(),
      vec![
        "file:///a.js",
        "file:///b.js",
        "file:///c.js",
        "file:///d.js"
      ]
    );

    let module_map_rc = JsRuntime::module_map(runtime.v8_isolate());
    let modules = module_map_rc.borrow();

    assert_eq!(
      modules.get_id("file:///a.js", AssertedModuleType::JavaScriptOrWasm),
      Some(a_id)
    );
    let b_id = modules
      .get_id("file:///b.js", AssertedModuleType::JavaScriptOrWasm)
      .unwrap();
    let c_id = modules
      .get_id("file:///c.js", AssertedModuleType::JavaScriptOrWasm)
      .unwrap();
    let d_id = modules
      .get_id("file:///d.js", AssertedModuleType::JavaScriptOrWasm)
      .unwrap();
    assert_eq!(
      modules.get_requested_modules(a_id),
      Some(&vec![
        ModuleRequest {
          specifier: "file:///b.js".to_string(),
          asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
        },
        ModuleRequest {
          specifier: "file:///c.js".to_string(),
          asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
        },
      ])
    );
    assert_eq!(
      modules.get_requested_modules(b_id),
      Some(&vec![ModuleRequest {
        specifier: "file:///c.js".to_string(),
        asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
      },])
    );
    assert_eq!(
      modules.get_requested_modules(c_id),
      Some(&vec![ModuleRequest {
        specifier: "file:///d.js".to_string(),
        asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
      },])
    );
    assert_eq!(modules.get_requested_modules(d_id), Some(&vec![]));
  }

  #[test]
  fn test_mods() {
    #[derive(Default)]
    struct ModsLoader {
      pub count: Arc<AtomicUsize>,
    }

    impl ModuleLoader for ModsLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
      ) -> Result<ModuleSpecifier, Error> {
        self.count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(specifier, "./b.js");
        assert_eq!(referrer, "file:///a.js");
        let s = resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        unreachable!()
      }
    }

    let loader = Rc::new(ModsLoader::default());

    let resolve_count = loader.count.clone();
    static DISPATCH_COUNT: AtomicUsize = AtomicUsize::new(0);

    #[op]
    fn op_test(control: u8) -> u8 {
      DISPATCH_COUNT.fetch_add(1, Ordering::Relaxed);
      assert_eq!(control, 42);
      43
    }

    let ext = Extension::builder("test_ext")
      .ops(vec![op_test::decl()])
      .build();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![ext],
      module_loader: Some(loader),
      ..Default::default()
    });

    runtime
      .execute_script(
        "setup.js",
        r#"
        function assert(cond) {
          if (!cond) {
            throw Error("assert");
          }
        }
        "#,
      )
      .unwrap();

    assert_eq!(DISPATCH_COUNT.load(Ordering::Relaxed), 0);

    let module_map_rc = JsRuntime::module_map(runtime.v8_isolate());

    let (mod_a, mod_b) = {
      let scope = &mut runtime.handle_scope();
      let mut module_map = module_map_rc.borrow_mut();
      let specifier_a = "file:///a.js".to_string();
      let mod_a = module_map
        .new_es_module(
          scope,
          true,
          &specifier_a,
          br#"
          import { b } from './b.js'
          if (b() != 'b') throw Error();
          let control = 42;
          Deno.core.ops.op_test(control);
        "#,
          false,
        )
        .unwrap();

      assert_eq!(DISPATCH_COUNT.load(Ordering::Relaxed), 0);
      let imports = module_map.get_requested_modules(mod_a);
      assert_eq!(
        imports,
        Some(&vec![ModuleRequest {
          specifier: "file:///b.js".to_string(),
          asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
        },])
      );

      let mod_b = module_map
        .new_es_module(
          scope,
          false,
          "file:///b.js",
          b"export function b() { return 'b' }",
          false,
        )
        .unwrap();
      let imports = module_map.get_requested_modules(mod_b).unwrap();
      assert_eq!(imports.len(), 0);
      (mod_a, mod_b)
    };

    runtime.instantiate_module(mod_b).unwrap();
    assert_eq!(DISPATCH_COUNT.load(Ordering::Relaxed), 0);
    assert_eq!(resolve_count.load(Ordering::SeqCst), 1);

    runtime.instantiate_module(mod_a).unwrap();
    assert_eq!(DISPATCH_COUNT.load(Ordering::Relaxed), 0);

    #[allow(clippy::let_underscore_future)]
    let _ = runtime.mod_evaluate(mod_a);
    assert_eq!(DISPATCH_COUNT.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn test_json_module() {
    #[derive(Default)]
    struct ModsLoader {
      pub count: Arc<AtomicUsize>,
    }

    impl ModuleLoader for ModsLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
      ) -> Result<ModuleSpecifier, Error> {
        self.count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(specifier, "./b.json");
        assert_eq!(referrer, "file:///a.js");
        let s = resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        unreachable!()
      }
    }

    let loader = Rc::new(ModsLoader::default());

    let resolve_count = loader.count.clone();

    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    runtime
      .execute_script(
        "setup.js",
        r#"
          function assert(cond) {
            if (!cond) {
              throw Error("assert");
            }
          }
          "#,
      )
      .unwrap();

    let module_map_rc = JsRuntime::module_map(runtime.v8_isolate());

    let (mod_a, mod_b) = {
      let scope = &mut runtime.handle_scope();
      let mut module_map = module_map_rc.borrow_mut();
      let specifier_a = "file:///a.js".to_string();
      let mod_a = module_map
        .new_es_module(
          scope,
          true,
          &specifier_a,
          br#"
            import jsonData from './b.json' assert {type: "json"};
            assert(jsonData.a == "b");
            assert(jsonData.c.d == 10);
          "#,
          false,
        )
        .unwrap();

      let imports = module_map.get_requested_modules(mod_a);
      assert_eq!(
        imports,
        Some(&vec![ModuleRequest {
          specifier: "file:///b.json".to_string(),
          asserted_module_type: AssertedModuleType::Json,
        },])
      );

      let mod_b = module_map
        .new_json_module(
          scope,
          "file:///b.json",
          b"{\"a\": \"b\", \"c\": {\"d\": 10}}",
        )
        .unwrap();
      let imports = module_map.get_requested_modules(mod_b).unwrap();
      assert_eq!(imports.len(), 0);
      (mod_a, mod_b)
    };

    runtime.instantiate_module(mod_b).unwrap();
    assert_eq!(resolve_count.load(Ordering::SeqCst), 1);

    runtime.instantiate_module(mod_a).unwrap();

    let receiver = runtime.mod_evaluate(mod_a);
    futures::executor::block_on(runtime.run_event_loop(false)).unwrap();
    futures::executor::block_on(receiver).unwrap().unwrap();
  }

  #[test]
  fn dyn_import_err() {
    #[derive(Clone, Default)]
    struct DynImportErrLoader {
      pub count: Arc<AtomicUsize>,
    }

    impl ModuleLoader for DynImportErrLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
      ) -> Result<ModuleSpecifier, Error> {
        self.count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(specifier, "/foo.js");
        assert_eq!(referrer, "file:///dyn_import2.js");
        let s = resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        async { Err(io::Error::from(io::ErrorKind::NotFound).into()) }.boxed()
      }
    }

    let loader = Rc::new(DynImportErrLoader::default());
    let count = loader.count.clone();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    // Test an erroneous dynamic import where the specified module isn't found.
    run_in_task(move |cx| {
      runtime
        .execute_script(
          "file:///dyn_import2.js",
          r#"
        (async () => {
          await import("/foo.js");
        })();
        "#,
        )
        .unwrap();

      // We should get an error here.
      let result = runtime.poll_event_loop(cx, false);
      if let Poll::Ready(Ok(_)) = result {
        unreachable!();
      }
      assert_eq!(count.load(Ordering::Relaxed), 4);
    })
  }

  #[derive(Clone, Default)]
  struct DynImportOkLoader {
    pub prepare_load_count: Arc<AtomicUsize>,
    pub resolve_count: Arc<AtomicUsize>,
    pub load_count: Arc<AtomicUsize>,
  }

  impl ModuleLoader for DynImportOkLoader {
    fn resolve(
      &self,
      specifier: &str,
      referrer: &str,
      _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, Error> {
      let c = self.resolve_count.fetch_add(1, Ordering::Relaxed);
      assert!(c < 7);
      assert_eq!(specifier, "./b.js");
      assert_eq!(referrer, "file:///dyn_import3.js");
      let s = resolve_import(specifier, referrer).unwrap();
      Ok(s)
    }

    fn load(
      &self,
      specifier: &ModuleSpecifier,
      _maybe_referrer: Option<ModuleSpecifier>,
      _is_dyn_import: bool,
    ) -> Pin<Box<ModuleSourceFuture>> {
      self.load_count.fetch_add(1, Ordering::Relaxed);
      let info = ModuleSource {
        module_url_specified: specifier.to_string(),
        module_url_found: specifier.to_string(),
        code: b"export function b() { return 'b' }"
          .to_vec()
          .into_boxed_slice(),
        module_type: ModuleType::JavaScript,
      };
      async move { Ok(info) }.boxed()
    }

    fn prepare_load(
      &self,
      _op_state: Rc<RefCell<OpState>>,
      _module_specifier: &ModuleSpecifier,
      _maybe_referrer: Option<String>,
      _is_dyn_import: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>>>> {
      self.prepare_load_count.fetch_add(1, Ordering::Relaxed);
      async { Ok(()) }.boxed_local()
    }
  }

  #[test]
  fn dyn_import_ok() {
    let loader = Rc::new(DynImportOkLoader::default());
    let prepare_load_count = loader.prepare_load_count.clone();
    let resolve_count = loader.resolve_count.clone();
    let load_count = loader.load_count.clone();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });
    run_in_task(move |cx| {
      // Dynamically import mod_b
      runtime
        .execute_script(
          "file:///dyn_import3.js",
          r#"
          (async () => {
            let mod = await import("./b.js");
            if (mod.b() !== 'b') {
              throw Error("bad1");
            }
            // And again!
            mod = await import("./b.js");
            if (mod.b() !== 'b') {
              throw Error("bad2");
            }
          })();
          "#,
        )
        .unwrap();

      assert!(matches!(
        runtime.poll_event_loop(cx, false),
        Poll::Ready(Ok(_))
      ));
      assert_eq!(prepare_load_count.load(Ordering::Relaxed), 1);
      assert_eq!(resolve_count.load(Ordering::Relaxed), 7);
      assert_eq!(load_count.load(Ordering::Relaxed), 1);
      assert!(matches!(
        runtime.poll_event_loop(cx, false),
        Poll::Ready(Ok(_))
      ));
      assert_eq!(resolve_count.load(Ordering::Relaxed), 7);
      assert_eq!(load_count.load(Ordering::Relaxed), 1);
    })
  }

  #[test]
  fn dyn_import_borrow_mut_error() {
    // https://github.com/denoland/deno/issues/6054
    let loader = Rc::new(DynImportOkLoader::default());
    let prepare_load_count = loader.prepare_load_count.clone();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    run_in_task(move |cx| {
      runtime
        .execute_script(
          "file:///dyn_import3.js",
          r#"
          (async () => {
            let mod = await import("./b.js");
            if (mod.b() !== 'b') {
              throw Error("bad");
            }
          })();
          "#,
        )
        .unwrap();
      // First poll runs `prepare_load` hook.
      let _ = runtime.poll_event_loop(cx, false);
      assert_eq!(prepare_load_count.load(Ordering::Relaxed), 1);
      // Second poll triggers error
      let _ = runtime.poll_event_loop(cx, false);
    })
  }

  // Regression test for https://github.com/denoland/deno/issues/3736.
  #[test]
  fn dyn_concurrent_circular_import() {
    #[derive(Clone, Default)]
    struct DynImportCircularLoader {
      pub resolve_count: Arc<AtomicUsize>,
      pub load_count: Arc<AtomicUsize>,
    }

    impl ModuleLoader for DynImportCircularLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
      ) -> Result<ModuleSpecifier, Error> {
        self.resolve_count.fetch_add(1, Ordering::Relaxed);
        let s = resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        self.load_count.fetch_add(1, Ordering::Relaxed);
        let filename = PathBuf::from(specifier.to_string())
          .file_name()
          .unwrap()
          .to_string_lossy()
          .to_string();
        let code = match filename.as_str() {
          "a.js" => "import './b.js';",
          "b.js" => "import './c.js';\nimport './a.js';",
          "c.js" => "import './d.js';",
          "d.js" => "// pass",
          _ => unreachable!(),
        };
        let info = ModuleSource {
          module_url_specified: specifier.to_string(),
          module_url_found: specifier.to_string(),
          code: code.as_bytes().to_vec().into_boxed_slice(),
          module_type: ModuleType::JavaScript,
        };
        async move { Ok(info) }.boxed()
      }
    }

    let loader = Rc::new(DynImportCircularLoader::default());
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    runtime
      .execute_script(
        "file:///entry.js",
        "import('./b.js');\nimport('./a.js');",
      )
      .unwrap();

    let result = futures::executor::block_on(runtime.run_event_loop(false));
    assert!(result.is_ok());
  }

  #[test]
  fn test_circular_load() {
    let loader = MockLoader::new();
    let loads = loader.loads.clone();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    let fut = async move {
      let spec = resolve_url("file:///circular1.js").unwrap();
      let result = runtime.load_main_module(&spec, None).await;
      assert!(result.is_ok());
      let circular1_id = result.unwrap();
      #[allow(clippy::let_underscore_future)]
      let _ = runtime.mod_evaluate(circular1_id);
      runtime.run_event_loop(false).await.unwrap();

      let l = loads.lock();
      assert_eq!(
        l.to_vec(),
        vec![
          "file:///circular1.js",
          "file:///circular2.js",
          "file:///circular3.js"
        ]
      );

      let module_map_rc = JsRuntime::module_map(runtime.v8_isolate());
      let modules = module_map_rc.borrow();

      assert_eq!(
        modules
          .get_id("file:///circular1.js", AssertedModuleType::JavaScriptOrWasm),
        Some(circular1_id)
      );
      let circular2_id = modules
        .get_id("file:///circular2.js", AssertedModuleType::JavaScriptOrWasm)
        .unwrap();

      assert_eq!(
        modules.get_requested_modules(circular1_id),
        Some(&vec![ModuleRequest {
          specifier: "file:///circular2.js".to_string(),
          asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
        }])
      );

      assert_eq!(
        modules.get_requested_modules(circular2_id),
        Some(&vec![ModuleRequest {
          specifier: "file:///circular3.js".to_string(),
          asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
        }])
      );

      assert!(modules
        .get_id("file:///circular3.js", AssertedModuleType::JavaScriptOrWasm)
        .is_some());
      let circular3_id = modules
        .get_id("file:///circular3.js", AssertedModuleType::JavaScriptOrWasm)
        .unwrap();
      assert_eq!(
        modules.get_requested_modules(circular3_id),
        Some(&vec![
          ModuleRequest {
            specifier: "file:///circular1.js".to_string(),
            asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
          },
          ModuleRequest {
            specifier: "file:///circular2.js".to_string(),
            asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
          }
        ])
      );
    }
    .boxed_local();

    futures::executor::block_on(fut);
  }

  #[test]
  fn test_redirect_load() {
    let loader = MockLoader::new();
    let loads = loader.loads.clone();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    let fut = async move {
      let spec = resolve_url("file:///redirect1.js").unwrap();
      let result = runtime.load_main_module(&spec, None).await;
      assert!(result.is_ok());
      let redirect1_id = result.unwrap();
      #[allow(clippy::let_underscore_future)]
      let _ = runtime.mod_evaluate(redirect1_id);
      runtime.run_event_loop(false).await.unwrap();
      let l = loads.lock();
      assert_eq!(
        l.to_vec(),
        vec![
          "file:///redirect1.js",
          "file:///redirect2.js",
          "file:///dir/redirect3.js"
        ]
      );

      let module_map_rc = JsRuntime::module_map(runtime.v8_isolate());
      let modules = module_map_rc.borrow();

      assert_eq!(
        modules
          .get_id("file:///redirect1.js", AssertedModuleType::JavaScriptOrWasm),
        Some(redirect1_id)
      );

      let redirect2_id = modules
        .get_id(
          "file:///dir/redirect2.js",
          AssertedModuleType::JavaScriptOrWasm,
        )
        .unwrap();
      assert!(modules.is_alias(
        "file:///redirect2.js",
        AssertedModuleType::JavaScriptOrWasm
      ));
      assert!(!modules.is_alias(
        "file:///dir/redirect2.js",
        AssertedModuleType::JavaScriptOrWasm
      ));
      assert_eq!(
        modules
          .get_id("file:///redirect2.js", AssertedModuleType::JavaScriptOrWasm),
        Some(redirect2_id)
      );

      let redirect3_id = modules
        .get_id("file:///redirect3.js", AssertedModuleType::JavaScriptOrWasm)
        .unwrap();
      assert!(modules.is_alias(
        "file:///dir/redirect3.js",
        AssertedModuleType::JavaScriptOrWasm
      ));
      assert!(!modules.is_alias(
        "file:///redirect3.js",
        AssertedModuleType::JavaScriptOrWasm
      ));
      assert_eq!(
        modules.get_id(
          "file:///dir/redirect3.js",
          AssertedModuleType::JavaScriptOrWasm
        ),
        Some(redirect3_id)
      );
    }
    .boxed_local();

    futures::executor::block_on(fut);
  }

  #[test]
  fn slow_never_ready_modules() {
    let loader = MockLoader::new();
    let loads = loader.loads.clone();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    run_in_task(move |cx| {
      let spec = resolve_url("file:///main.js").unwrap();
      let mut recursive_load =
        runtime.load_main_module(&spec, None).boxed_local();

      let result = recursive_load.poll_unpin(cx);
      assert!(result.is_pending());

      // TODO(ry) Arguably the first time we poll only the following modules
      // should be loaded:
      //      "file:///main.js",
      //      "file:///never_ready.js",
      //      "file:///slow.js"
      // But due to current task notification in DelayedSourceCodeFuture they
      // all get loaded in a single poll. Also see the comment above
      // run_in_task.

      for _ in 0..10 {
        let result = recursive_load.poll_unpin(cx);
        assert!(result.is_pending());
        let l = loads.lock();
        assert_eq!(
          l.to_vec(),
          vec![
            "file:///main.js",
            "file:///never_ready.js",
            "file:///slow.js",
            "file:///a.js",
            "file:///b.js",
            "file:///c.js",
            "file:///d.js"
          ]
        );
      }
    })
  }

  #[test]
  fn loader_disappears_after_error() {
    let loader = MockLoader::new();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    run_in_task(move |cx| {
      let spec = resolve_url("file:///bad_import.js").unwrap();
      let mut load_fut = runtime.load_main_module(&spec, None).boxed_local();
      let result = load_fut.poll_unpin(cx);
      if let Poll::Ready(Err(err)) = result {
        assert_eq!(
          err.downcast_ref::<MockError>().unwrap(),
          &MockError::ResolveErr
        );
      } else {
        unreachable!();
      }
    })
  }

  #[test]
  fn recursive_load_main_with_code() {
    const MAIN_WITH_CODE_SRC: &str = r#"
import { b } from "/b.js";
import { c } from "/c.js";
if (b() != 'b') throw Error();
if (c() != 'c') throw Error();
if (!import.meta.main) throw Error();
if (import.meta.url != 'file:///main_with_code.js') throw Error();
"#;

    let loader = MockLoader::new();
    let loads = loader.loads.clone();
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });
    // In default resolution code should be empty.
    // Instead we explicitly pass in our own code.
    // The behavior should be very similar to /a.js.
    let spec = resolve_url("file:///main_with_code.js").unwrap();
    let main_id_fut = runtime
      .load_main_module(&spec, Some(MAIN_WITH_CODE_SRC.to_owned()))
      .boxed_local();
    let main_id = futures::executor::block_on(main_id_fut).unwrap();

    #[allow(clippy::let_underscore_future)]
    let _ = runtime.mod_evaluate(main_id);
    futures::executor::block_on(runtime.run_event_loop(false)).unwrap();

    let l = loads.lock();
    assert_eq!(
      l.to_vec(),
      vec!["file:///b.js", "file:///c.js", "file:///d.js"]
    );

    let module_map_rc = JsRuntime::module_map(runtime.v8_isolate());
    let modules = module_map_rc.borrow();

    assert_eq!(
      modules.get_id(
        "file:///main_with_code.js",
        AssertedModuleType::JavaScriptOrWasm
      ),
      Some(main_id)
    );
    let b_id = modules
      .get_id("file:///b.js", AssertedModuleType::JavaScriptOrWasm)
      .unwrap();
    let c_id = modules
      .get_id("file:///c.js", AssertedModuleType::JavaScriptOrWasm)
      .unwrap();
    let d_id = modules
      .get_id("file:///d.js", AssertedModuleType::JavaScriptOrWasm)
      .unwrap();

    assert_eq!(
      modules.get_requested_modules(main_id),
      Some(&vec![
        ModuleRequest {
          specifier: "file:///b.js".to_string(),
          asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
        },
        ModuleRequest {
          specifier: "file:///c.js".to_string(),
          asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
        }
      ])
    );
    assert_eq!(
      modules.get_requested_modules(b_id),
      Some(&vec![ModuleRequest {
        specifier: "file:///c.js".to_string(),
        asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
      }])
    );
    assert_eq!(
      modules.get_requested_modules(c_id),
      Some(&vec![ModuleRequest {
        specifier: "file:///d.js".to_string(),
        asserted_module_type: AssertedModuleType::JavaScriptOrWasm,
      }])
    );
    assert_eq!(modules.get_requested_modules(d_id), Some(&vec![]));
  }

  #[test]
  fn main_and_side_module() {
    struct ModsLoader {}

    let main_specifier = resolve_url("file:///main_module.js").unwrap();
    let side_specifier = resolve_url("file:///side_module.js").unwrap();

    impl ModuleLoader for ModsLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
      ) -> Result<ModuleSpecifier, Error> {
        let s = resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        let module_source = match module_specifier.as_str() {
          "file:///main_module.js" => Ok(ModuleSource {
            module_url_specified: "file:///main_module.js".to_string(),
            module_url_found: "file:///main_module.js".to_string(),
            code: b"if (!import.meta.main) throw Error();"
              .to_vec()
              .into_boxed_slice(),
            module_type: ModuleType::JavaScript,
          }),
          "file:///side_module.js" => Ok(ModuleSource {
            module_url_specified: "file:///side_module.js".to_string(),
            module_url_found: "file:///side_module.js".to_string(),
            code: b"if (import.meta.main) throw Error();"
              .to_vec()
              .into_boxed_slice(),
            module_type: ModuleType::JavaScript,
          }),
          _ => unreachable!(),
        };
        async move { module_source }.boxed()
      }
    }

    let loader = Rc::new(ModsLoader {});
    let mut runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(loader),
      ..Default::default()
    });

    let main_id_fut = runtime
      .load_main_module(&main_specifier, None)
      .boxed_local();
    let main_id = futures::executor::block_on(main_id_fut).unwrap();

    #[allow(clippy::let_underscore_future)]
    let _ = runtime.mod_evaluate(main_id);
    futures::executor::block_on(runtime.run_event_loop(false)).unwrap();

    // Try to add another main module - it should error.
    let side_id_fut = runtime
      .load_main_module(&side_specifier, None)
      .boxed_local();
    futures::executor::block_on(side_id_fut).unwrap_err();

    // And now try to load it as a side module
    let side_id_fut = runtime
      .load_side_module(&side_specifier, None)
      .boxed_local();
    let side_id = futures::executor::block_on(side_id_fut).unwrap();

    #[allow(clippy::let_underscore_future)]
    let _ = runtime.mod_evaluate(side_id);
    futures::executor::block_on(runtime.run_event_loop(false)).unwrap();
  }

  #[test]
  fn dynamic_imports_snapshot() {
    //TODO: Once the issue with the ModuleNamespaceEntryGetter is fixed, we can maintain a reference to the module
    // and use it when loading the snapshot
    let snapshot = {
      const MAIN_WITH_CODE_SRC: &str = r#"
      await import("./b.js");
    "#;

      let loader = MockLoader::new();
      let mut runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(loader),
        will_snapshot: true,
        ..Default::default()
      });
      // In default resolution code should be empty.
      // Instead we explicitly pass in our own code.
      // The behavior should be very similar to /a.js.
      let spec = resolve_url("file:///main_with_code.js").unwrap();
      let main_id_fut = runtime
        .load_main_module(&spec, Some(MAIN_WITH_CODE_SRC.to_owned()))
        .boxed_local();
      let main_id = futures::executor::block_on(main_id_fut).unwrap();

      #[allow(clippy::let_underscore_future)]
      let _ = runtime.mod_evaluate(main_id);
      futures::executor::block_on(runtime.run_event_loop(false)).unwrap();
      runtime.snapshot()
    };

    let snapshot = Snapshot::JustCreated(snapshot);
    let mut runtime2 = JsRuntime::new(RuntimeOptions {
      startup_snapshot: Some(snapshot),
      ..Default::default()
    });

    //Evaluate the snapshot with an empty function
    runtime2.execute_script("check.js", "true").unwrap();
  }

  #[test]
  fn import_meta_snapshot() {
    let snapshot = {
      const MAIN_WITH_CODE_SRC: &str = r#"
    if (import.meta.url != 'file:///main_with_code.js') throw Error();
    globalThis.meta = import.meta;
    globalThis.url = import.meta.url;
    "#;

      let loader = MockLoader::new();
      let mut runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(loader),
        will_snapshot: true,
        ..Default::default()
      });
      // In default resolution code should be empty.
      // Instead we explicitly pass in our own code.
      // The behavior should be very similar to /a.js.
      let spec = resolve_url("file:///main_with_code.js").unwrap();
      let main_id_fut = runtime
        .load_main_module(&spec, Some(MAIN_WITH_CODE_SRC.to_owned()))
        .boxed_local();
      let main_id = futures::executor::block_on(main_id_fut).unwrap();

      #[allow(clippy::let_underscore_future)]
      let _ = runtime.mod_evaluate(main_id);
      futures::executor::block_on(runtime.run_event_loop(false)).unwrap();
      runtime.snapshot()
    };

    let snapshot = Snapshot::JustCreated(snapshot);
    let mut runtime2 = JsRuntime::new(RuntimeOptions {
      startup_snapshot: Some(snapshot),
      ..Default::default()
    });

    runtime2
      .execute_script(
        "check.js",
        "if (globalThis.url !== 'file:///main_with_code.js') throw Error('x')",
      )
      .unwrap();
  }

  #[test]
  fn internal_module_loader() {
    let loader = InternalModuleLoader::default();
    assert!(loader
      .resolve("internal:foo", "internal:bar", ResolutionKind::Import)
      .is_ok());
    assert_eq!(
      loader
        .resolve("internal:foo", "file://bar", ResolutionKind::Import)
        .err()
        .map(|e| e.to_string()),
      Some("Cannot load internal module from external code".to_string())
    );
    assert_eq!(
      loader
        .resolve("file://foo", "file://bar", ResolutionKind::Import)
        .err()
        .map(|e| e.to_string()),
      Some(
        "Module loading is not supported; attempted to resolve: \"file://foo\" from \"file://bar\""
          .to_string()
      )
    );
    assert_eq!(
      loader
        .resolve("file://foo", "internal:bar", ResolutionKind::Import)
        .err()
        .map(|e| e.to_string()),
      Some(
        "Module loading is not supported; attempted to resolve: \"file://foo\" from \"internal:bar\""
        .to_string()
      )
    );
    assert_eq!(
      resolve_helper(
        true,
        Rc::new(loader),
        "internal:core.js",
        "file://bar",
        ResolutionKind::Import,
      )
      .err()
      .map(|e| e.to_string()),
      Some("Cannot load internal module from external code".to_string())
    );
  }
}
