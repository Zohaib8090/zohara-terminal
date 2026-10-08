//! `ut-plugin-loader`: actually loading a `.so` behind `ut-plugin-signing`'s
//! verification (Section 19 "Plugin and developer platform" — this is the
//! piece the roadmap flagged as missing: signing checks a manifest, nothing
//! loaded code *behind* that check).
//!
//! The obvious design — export `extern "C" fn create() -> *mut dyn Plugin`
//! and hand the host a live Rust trait object — was deliberately rejected.
//! A `dyn Trait` pointer is a fat pointer whose vtable layout is an
//! implementation detail of one specific rustc build; it happens to work
//! when host and plugin share a toolchain and crate graph, and breaks
//! silently (wrong vtable, UB, not a clean error) the moment either
//! changes. That is not a foundation to build a plugin ecosystem on.
//!
//! Instead this crate defines a small, genuinely stable C ABI: five
//! `extern "C"` functions operating on an opaque `*mut c_void` instance and
//! plain C strings. A plugin author never touches this directly — they
//! `impl DynPlugin for MyPlugin` (a `Send`-bound subset of
//! `ut_plugin_api::Plugin` with no borrowed `PluginContext`, since a
//! borrow can't cross a dylib boundary safely) and call
//! [`export_plugin!`] once. [`Action`]s cross the boundary as a tiny
//! tagged string protocol ([`encode_action`]/[`decode_action`]), not as
//! raw enum bytes, so the wire format is stable even if `PluginAction`'s
//! internal representation ever changes.
//!
//! What a dynamically-loaded plugin can *not* do, by design, in this first
//! version: see `recent_output`/`cwd`/`check_command_safety` via a
//! `PluginContext`. It only ever receives the one command or output chunk
//! string for that call and returns an `Action` — a strictly narrower
//! capability than an in-process plugin gets. Widening this means adding
//! more plain-data parameters to the ABI functions, not handing out a
//! context object across the boundary.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;

use ut_plugin_api::PluginAction;
use ut_plugin_signing::{Keyring, SignedManifest, SigningError};

/// Bump this whenever the ABI (function signatures, string protocol)
/// changes incompatibly. The host refuses to load a plugin whose
/// `zohara_plugin_abi_version()` doesn't match, rather than guessing.
pub const ABI_VERSION: u32 = 1;

/// The reduced capability surface a dynamically-loaded plugin implements.
/// Deliberately not the full `ut_plugin_api::Plugin` trait — see the
/// module doc for why no `PluginContext` crosses the boundary.
pub trait DynPlugin: Send {
    fn id(&self) -> String;
    fn on_command(&mut self, _command: &str) -> PluginAction {
        PluginAction::None
    }
    fn on_output(&mut self, _chunk: &str) -> PluginAction {
        PluginAction::None
    }
}

/// Encode a `PluginAction` as the wire string a dynamic plugin returns.
/// Uses a unit-separator byte (`\x1f`) between tag and payload so the
/// payload itself can contain any character (including `:`) without
/// ambiguity.
pub fn encode_action(action: &PluginAction) -> String {
    match action {
        PluginAction::None => "NONE".to_string(),
        PluginAction::SuggestCommand(s) => format!("SUGGEST\u{1f}{s}"),
        PluginAction::ShowMessage(s) => format!("MESSAGE\u{1f}{s}"),
        PluginAction::RunCommand(s) => format!("RUN\u{1f}{s}"),
    }
}

/// Inverse of [`encode_action`]. An unrecognized tag decodes to `None`
/// rather than panicking or erroring — a forward-compatible plugin
/// shouldn't be able to crash an older host by returning a new tag.
pub fn decode_action(wire: &str) -> PluginAction {
    match wire.split_once('\u{1f}') {
        Some(("SUGGEST", rest)) => PluginAction::SuggestCommand(rest.to_string()),
        Some(("MESSAGE", rest)) => PluginAction::ShowMessage(rest.to_string()),
        Some(("RUN", rest)) => PluginAction::RunCommand(rest.to_string()),
        _ => PluginAction::None,
    }
}

/// Allocate a C string a host must free via the plugin's
/// `zohara_plugin_free_string`. Used only by [`export_plugin!`]'s
/// generated code.
pub fn string_to_c(s: String) -> *mut c_char {
    CString::new(s.replace('\0', "")).expect("no interior nul after stripping").into_raw()
}

/// Read a borrowed C string from the host without taking ownership. Used
/// only by [`export_plugin!`]'s generated code.
///
/// # Safety
/// `ptr` must be a valid, nul-terminated, UTF-8 C string for the duration
/// of this call (the host guarantees this for every ABI call it makes).
pub unsafe fn c_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    CStr::from_ptr(ptr).to_string_lossy().into_owned()
}

/// Generates the five `extern "C"` ABI functions for a plugin crate built
/// as a `cdylib`. `$ty` must implement `DynPlugin + Default`.
#[macro_export]
macro_rules! export_plugin {
    ($ty:ty) => {
        #[no_mangle]
        pub extern "C" fn zohara_plugin_abi_version() -> u32 {
            $crate::ABI_VERSION
        }

        #[no_mangle]
        pub extern "C" fn zohara_plugin_create() -> *mut ::std::ffi::c_void {
            let instance: Box<dyn $crate::DynPlugin> = Box::new(<$ty as ::std::default::Default>::default());
            Box::into_raw(Box::new(instance)) as *mut ::std::ffi::c_void
        }

        /// # Safety
        /// `ptr` must be a value previously returned by this module's
        /// `zohara_plugin_create`, not yet destroyed.
        #[no_mangle]
        pub unsafe extern "C" fn zohara_plugin_destroy(ptr: *mut ::std::ffi::c_void) {
            if !ptr.is_null() {
                drop(Box::from_raw(ptr as *mut Box<dyn $crate::DynPlugin>));
            }
        }

        /// # Safety
        /// `ptr` must be a live value from `zohara_plugin_create`.
        #[no_mangle]
        pub unsafe extern "C" fn zohara_plugin_id(ptr: *mut ::std::ffi::c_void) -> *mut ::std::ffi::c_char {
            let instance = &*(ptr as *mut Box<dyn $crate::DynPlugin>);
            $crate::string_to_c(instance.id())
        }

        /// # Safety
        /// `ptr` must be a live value from `zohara_plugin_create`; `command`
        /// must be a valid nul-terminated UTF-8 C string (or null).
        #[no_mangle]
        pub unsafe extern "C" fn zohara_plugin_on_command(
            ptr: *mut ::std::ffi::c_void,
            command: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            let instance = &mut *(ptr as *mut Box<dyn $crate::DynPlugin>);
            let command = $crate::c_to_string(command);
            let action = instance.on_command(&command);
            $crate::string_to_c($crate::encode_action(&action))
        }

        /// # Safety
        /// `ptr` must be a live value from `zohara_plugin_create`; `chunk`
        /// must be a valid nul-terminated UTF-8 C string (or null).
        #[no_mangle]
        pub unsafe extern "C" fn zohara_plugin_on_output(
            ptr: *mut ::std::ffi::c_void,
            chunk: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            let instance = &mut *(ptr as *mut Box<dyn $crate::DynPlugin>);
            let chunk = $crate::c_to_string(chunk);
            let action = instance.on_output(&chunk);
            $crate::string_to_c($crate::encode_action(&action))
        }

        /// # Safety
        /// `s` must be a pointer this module itself returned from one of
        /// its `*mut c_char`-returning functions, not yet freed.
        #[no_mangle]
        pub unsafe extern "C" fn zohara_plugin_free_string(s: *mut ::std::ffi::c_char) {
            if !s.is_null() {
                drop(::std::ffi::CString::from_raw(s));
            }
        }
    };
}

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type CreateFn = unsafe extern "C" fn() -> *mut c_void;
type DestroyFn = unsafe extern "C" fn(*mut c_void);
type IdFn = unsafe extern "C" fn(*mut c_void) -> *mut c_char;
type OnStringFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type FreeStringFn = unsafe extern "C" fn(*mut c_char);

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read plugin artifact: {0}")]
    Io(#[from] std::io::Error),
    #[error("signature/manifest rejected: {0}")]
    Rejected(SigningError),
    #[error("failed to open dynamic library: {0}")]
    Open(libloading::Error),
    #[error("plugin is missing required ABI symbol {0:?}: {1}")]
    MissingSymbol(&'static str, libloading::Error),
    #[error("plugin ABI version {got} is incompatible with host ABI version {expected}")]
    AbiVersionMismatch { expected: u32, got: u32 },
}

/// A loaded, live dynamic plugin. Dropping it calls the plugin's destroy
/// function *before* the underlying `Library` unloads (the explicit `Drop`
/// impl's body runs first, then fields drop in declaration order) — the
/// destroy call's code lives in that library, so the order matters.
pub struct LoadedPlugin {
    _lib: libloading::Library,
    instance: *mut c_void,
    destroy: DestroyFn,
    id_fn: IdFn,
    on_command_fn: OnStringFn,
    on_output_fn: OnStringFn,
    free_string: FreeStringFn,
}

// Safety: the only mutable state crossed per call is the plugin's own
// `*mut c_void` instance, accessed through one `&mut` at a time on the
// loader side (same discipline as any other `Box<dyn Trait>` owner); the
// plugin author's `DynPlugin: Send` bound is what actually makes moving a
// `LoadedPlugin` across threads sound.
unsafe impl Send for LoadedPlugin {}

impl LoadedPlugin {
    pub fn id(&self) -> String {
        unsafe {
            let raw = (self.id_fn)(self.instance);
            let s = c_to_string(raw);
            (self.free_string)(raw);
            s
        }
    }

    pub fn on_command(&mut self, command: &str) -> PluginAction {
        self.call_on_string(self.on_command_fn, command)
    }

    pub fn on_output(&mut self, chunk: &str) -> PluginAction {
        self.call_on_string(self.on_output_fn, chunk)
    }

    fn call_on_string(&mut self, f: OnStringFn, input: &str) -> PluginAction {
        let c_input = CString::new(input.replace('\0', "")).expect("no interior nul after stripping");
        unsafe {
            let raw = f(self.instance, c_input.as_ptr());
            let wire = c_to_string(raw);
            (self.free_string)(raw);
            decode_action(&wire)
        }
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.instance);
        }
    }
}

/// Load a plugin `.so` with no signature check. Only meant for tests and
/// for a caller that has already verified trust some other way — real
/// plugin installation should go through [`load_verified`].
///
/// # Safety
/// Loading and calling into arbitrary native code is inherently unsafe:
/// the caller is asserting `path` is a well-formed plugin built against
/// this crate's ABI (ideally proven by [`load_verified`]'s signature
/// check), not just any shared library.
pub unsafe fn load(path: &Path) -> Result<LoadedPlugin, LoadError> {
    let lib = libloading::Library::new(path).map_err(LoadError::Open)?;

    let abi_version: libloading::Symbol<AbiVersionFn> =
        lib.get(b"zohara_plugin_abi_version\0").map_err(|e| LoadError::MissingSymbol("zohara_plugin_abi_version", e))?;
    let got = abi_version();
    if got != ABI_VERSION {
        return Err(LoadError::AbiVersionMismatch { expected: ABI_VERSION, got });
    }

    let create: libloading::Symbol<CreateFn> =
        lib.get(b"zohara_plugin_create\0").map_err(|e| LoadError::MissingSymbol("zohara_plugin_create", e))?;
    let destroy: libloading::Symbol<DestroyFn> =
        lib.get(b"zohara_plugin_destroy\0").map_err(|e| LoadError::MissingSymbol("zohara_plugin_destroy", e))?;
    let id_fn: libloading::Symbol<IdFn> =
        lib.get(b"zohara_plugin_id\0").map_err(|e| LoadError::MissingSymbol("zohara_plugin_id", e))?;
    let on_command_fn: libloading::Symbol<OnStringFn> =
        lib.get(b"zohara_plugin_on_command\0").map_err(|e| LoadError::MissingSymbol("zohara_plugin_on_command", e))?;
    let on_output_fn: libloading::Symbol<OnStringFn> =
        lib.get(b"zohara_plugin_on_output\0").map_err(|e| LoadError::MissingSymbol("zohara_plugin_on_output", e))?;
    let free_string: libloading::Symbol<FreeStringFn> =
        lib.get(b"zohara_plugin_free_string\0").map_err(|e| LoadError::MissingSymbol("zohara_plugin_free_string", e))?;

    let instance = create();

    Ok(LoadedPlugin {
        destroy: *destroy,
        id_fn: *id_fn,
        on_command_fn: *on_command_fn,
        on_output_fn: *on_output_fn,
        free_string: *free_string,
        instance,
        _lib: lib,
    })
}

/// Load a plugin `.so`, but only after its bytes pass
/// `ut-plugin-signing::Keyring::verify` against `signed` — the step the
/// roadmap flagged as missing. A manifest whose `artifact_sha256` doesn't
/// match these exact bytes, or whose signature isn't from a trusted key,
/// is rejected before `libloading` ever touches the file.
///
/// # Safety
/// Same caveat as [`load`]: this executes native code. Verification
/// proves the artifact is untampered and from a trusted publisher, not
/// that the publisher's code is itself bug-free or non-malicious — trust
/// still has to be extended to a publisher deliberately, via `Keyring`.
pub unsafe fn load_verified(path: &Path, signed: &SignedManifest, keyring: &Keyring) -> Result<LoadedPlugin, LoadError> {
    let bytes = std::fs::read(path)?;
    keyring.verify(signed, Some(&bytes)).map_err(LoadError::Rejected)?;
    load(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_round_trips_through_the_wire_protocol() {
        for action in [
            PluginAction::None,
            PluginAction::SuggestCommand("ls -la".to_string()),
            PluginAction::ShowMessage("hello: world".to_string()),
            PluginAction::RunCommand("echo hi".to_string()),
        ] {
            assert_eq!(decode_action(&encode_action(&action)), action);
        }
    }

    #[test]
    fn unrecognized_wire_tag_decodes_to_none_rather_than_panicking() {
        assert_eq!(decode_action("GARBAGE"), PluginAction::None);
        assert_eq!(decode_action(""), PluginAction::None);
    }

    #[test]
    fn loading_a_nonexistent_file_fails_cleanly() {
        let err = unsafe { load(Path::new("/nonexistent/not-a-real-plugin.so")) };
        assert!(matches!(err, Err(LoadError::Open(_))));
    }

    #[test]
    fn verification_rejects_before_any_library_is_opened() {
        use ut_plugin_signing::{hash_artifact, Manifest, PluginKeypair};

        let keypair = PluginKeypair::generate();
        let manifest = Manifest {
            id: "fake".into(),
            name: "Fake".into(),
            version: "1.0.0".into(),
            author: "nobody".into(),
            requested_capabilities: vec![],
            artifact_sha256: hash_artifact(b"expected bytes"),
        };
        let signed = keypair.sign(manifest);
        let keyring = Keyring::new(); // key not trusted

        // Path doesn't even need to exist: verification runs first and
        // fails on the untrusted key before `std::fs::read` would matter
        // for the *library open* step (it still reads the file to hash
        // it, so point it at a real small file).
        let tmp = std::env::temp_dir().join("ut-plugin-loader-verify-test.bin");
        std::fs::write(&tmp, b"expected bytes").unwrap();
        let result = unsafe { load_verified(&tmp, &signed, &keyring) };
        let _ = std::fs::remove_file(&tmp);
        assert!(matches!(result, Err(LoadError::Rejected(SigningError::UntrustedKey))));
    }

    #[test]
    fn verification_rejects_tampered_artifact_bytes() {
        use ut_plugin_signing::{hash_artifact, Manifest, PluginKeypair};

        let keypair = PluginKeypair::generate();
        let manifest = Manifest {
            id: "fake".into(),
            name: "Fake".into(),
            version: "1.0.0".into(),
            author: "nobody".into(),
            requested_capabilities: vec![],
            artifact_sha256: hash_artifact(b"original bytes"),
        };
        let signed = keypair.sign(manifest);
        let mut keyring = Keyring::new();
        keyring.trust(&keypair.public_key_hex(), "official").unwrap();

        let tmp = std::env::temp_dir().join("ut-plugin-loader-tamper-test.bin");
        std::fs::write(&tmp, b"tampered bytes!!").unwrap();
        let result = unsafe { load_verified(&tmp, &signed, &keyring) };
        let _ = std::fs::remove_file(&tmp);
        assert!(matches!(result, Err(LoadError::Rejected(SigningError::ArtifactMismatch))));
    }

    /// The real end-to-end proof: build the fixture plugin crate as a
    /// `cdylib`, sign its actual compiled bytes, trust the signer, and
    /// load + drive it through the full verified path — an actual `.so`
    /// loaded with `dlopen`, not a mock. Mirrors this repo's standing
    /// practice of proving infra-dependent features against something
    /// real (a local sshd, a real interpreter, a real screenshot).
    #[test]
    fn loads_and_drives_a_real_compiled_plugin_end_to_end() {
        use ut_plugin_signing::{hash_artifact, Manifest, PluginKeypair};

        let so_path = build_fixture_cdylib();
        let bytes = std::fs::read(&so_path).expect("read built fixture cdylib");

        let keypair = PluginKeypair::generate();
        let manifest = Manifest {
            id: "echo-fixture".into(),
            name: "Echo Fixture".into(),
            version: "1.0.0".into(),
            author: "test".into(),
            requested_capabilities: vec![],
            artifact_sha256: hash_artifact(&bytes),
        };
        let signed = keypair.sign(manifest);
        let mut keyring = Keyring::new();
        keyring.trust(&keypair.public_key_hex(), "official").unwrap();

        let mut plugin = unsafe { load_verified(&so_path, &signed, &keyring).expect("load verified fixture plugin") };

        assert_eq!(plugin.id(), "echo-fixture");
        assert_eq!(plugin.on_command("deploy prod"), PluginAction::SuggestCommand("echo: deploy prod".to_string()));
        assert_eq!(plugin.on_output("build finished"), PluginAction::ShowMessage("saw output: build finished".to_string()));
    }

    /// Builds `ut-plugin-loader-fixture` as a cdylib via a real `cargo
    /// build` subprocess and returns the path to the resulting shared
    /// object. Panics with the captured cargo output on failure so a
    /// broken fixture build fails loudly rather than as a confusing
    /// downstream symbol-lookup error.
    fn build_fixture_cdylib() -> std::path::PathBuf {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir.parent().and_then(|p| p.parent()).and_then(|p| p.parent()).expect("workspace root");

        let output = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "ut-plugin-loader-fixture", "--lib"])
            .current_dir(workspace_root)
            .output()
            .expect("run cargo build for fixture crate");
        assert!(
            output.status.success(),
            "fixture cdylib build failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let target_dir = workspace_root.join("target/debug");
        for name in ["libut_plugin_loader_fixture.so", "libut_plugin_loader_fixture.dylib", "ut_plugin_loader_fixture.dll"] {
            let candidate = target_dir.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
        panic!("built fixture crate but couldn't find its cdylib under {}", target_dir.display());
    }
}
