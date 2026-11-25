//! Loading a C library at run time, so the app starts and works with sticks and drives on a
//! machine that doesn't have the phone libraries. See DESIGN.md section 10.3.
//!
//! [`dyn_api!`] is the only way the phone and camera modules reach a library. Each library is opened once,
//! the first time it is needed, and every function the module names is looked up right then. If
//! the library, or any one function, is missing, that library is "not available": the wrappers
//! are never reached, because the module checks first and lists nothing.
//!
//! The functions named in a `dyn_api!` block are the only symbols ever looked up. The modules'
//! tests check that list against an allow-list of reading functions.

use std::ffi::{CStr, c_void};

/// An open shared library. Never closed: the functions looked up from it live for the whole run.
pub struct Lib(*mut c_void);

// SAFETY: a `dlopen` handle is only ever passed to `dlsym`, which is thread-safe.
unsafe impl Send for Lib {}
// SAFETY: as above.
unsafe impl Sync for Lib {}

/// Test hook (debug builds only): pretend no phone or camera library can be found. It covers
/// every library opened through [`Lib::open`]: libmtp, libimobiledevice and libgphoto2.
fn hidden() -> bool {
    cfg!(debug_assertions) && std::env::var_os("PNK_HIDE_PHONE_LIBS").is_some()
}

impl Lib {
    /// Opens the first of `names` that the system can find.
    pub fn open(names: &[&str]) -> Result<Lib, String> {
        if hidden() {
            return Err("hidden by PNK_HIDE_PHONE_LIBS".into());
        }
        for name in names {
            let Ok(c) = std::ffi::CString::new(*name) else {
                continue;
            };
            // SAFETY: `c` is a valid NUL-ended string for the call.
            let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !h.is_null() {
                return Ok(Lib(h));
            }
        }
        Err(format!("none of {names:?} could be loaded"))
    }

    /// Looks up one function. `name` must end with a NUL byte.
    pub fn sym(&self, name: &str) -> Result<*mut c_void, String> {
        let c = CStr::from_bytes_with_nul(name.as_bytes())
            .map_err(|_| format!("bad symbol name {name:?}"))?;
        // SAFETY: the handle is a live `dlopen` handle and `c` is NUL-ended.
        let p = unsafe { libc::dlsym(self.0, c.as_ptr()) };
        if p.is_null() {
            Err(format!("{} is missing", c.to_string_lossy()))
        } else {
            Ok(p)
        }
    }
}

/// Declares one library's functions (reading ones only: see the allow-list tests) and makes a
/// wrapper for each that calls the loaded function. The wrappers may only be called once
/// `$get()` has returned `Some`, which the module's `list` and `open` check first.
macro_rules! dyn_api {
    (
        sonames: $sonames:expr,
        off: $off:expr,
        api: $api:ident,
        get: $get:ident,
        load: $load:ident;
        $( pub fn $name:ident( $($arg:ident : $ty:ty),* $(,)? ) $(-> $ret:ty)? ; )*
    ) => {
        /// The looked-up functions.
        #[allow(non_snake_case)]
        pub struct $api {
            $( $name: unsafe extern "C" fn($($ty),*) $(-> $ret)?, )*
        }

        /// Opens the library from `names` and looks up every function; any failure is an error.
        #[allow(non_snake_case)]
        pub fn $load(names: &[&str]) -> Result<$api, String> {
            let lib = $crate::platform::linux::dynlib::Lib::open(names)?;
            Ok($api {
                $( $name: {
                    let p = lib.sym(concat!(stringify!($name), "\0"))?;
                    // SAFETY: the declaration above is the function's C signature.
                    unsafe {
                        std::mem::transmute::<*mut std::ffi::c_void, unsafe extern "C" fn($($ty),*) $(-> $ret)?>(p)
                    }
                }, )*
            })
        }

        /// The library's functions, loaded on first use, or `None` if it can't be (logged once).
        pub fn $get() -> Option<&'static $api> {
            static CELL: std::sync::OnceLock<Option<$api>> = std::sync::OnceLock::new();
            CELL.get_or_init(|| match $load($sonames) {
                Ok(api) => Some(api),
                Err(e) => {
                    log::info!("{} ({e})", $off);
                    None
                }
            })
            .as_ref()
        }

        $(
            /// # Safety
            /// As the C function, and the library must be available.
            #[allow(clippy::missing_safety_doc, non_snake_case)]
            pub unsafe fn $name( $($arg : $ty),* ) $(-> $ret)? {
                let api = $get().expect("the library was checked before use");
                // SAFETY: forwarded to the caller.
                unsafe { (api.$name)($($arg),*) }
            }
        )*
    };
}
pub(crate) use dyn_api;

/// What the phone modules' read-only tests share (DESIGN.md section 10.3).
#[cfg(test)]
pub(super) mod check {
    /// Names that must never appear in a phone module's code: anything that writes, sends,
    /// deletes, renames or sets on a phone, whether looked up or just mentioned.
    pub const FORBIDDEN: &[&str] = &[
        "LIBMTP_Send_File",
        "LIBMTP_Send_File_From_File",
        "LIBMTP_Send_File_From_Handler",
        "LIBMTP_Send_Track_From_File",
        "LIBMTP_Delete_Object",
        "LIBMTP_Create_Folder",
        "LIBMTP_Set_File_Name",
        "LIBMTP_Set_Object_Filename",
        "LIBMTP_Set_Friendlyname",
        "LIBMTP_Update_Track_Metadata",
        "LIBMTP_Format_Storage",
        "LIBMTP_Set_Device_Name",
        "afc_file_write",
        "afc_file_truncate",
        "afc_remove_path",
        "afc_remove_path_and_contents",
        "afc_rename_path",
        "afc_make_directory",
        "afc_make_link",
        "afc_set_file_time",
        "AFC_FOPEN_WRONLY",
        "AFC_FOPEN_RW",
        "AFC_FOPEN_WR",
        "AFC_FOPEN_APPEND",
        // libgphoto2: anything that deletes, uploads, makes or removes a folder, captures,
        // changes a setting or a file's information, or reads with a raw offset.
        "gp_camera_file_delete",
        "gp_camera_folder_put_file",
        "gp_camera_folder_delete_all",
        "gp_camera_folder_make_dir",
        "gp_camera_folder_remove_dir",
        "gp_camera_set_config",
        "gp_camera_set_single_config",
        "gp_camera_capture",
        "gp_camera_capture_preview",
        "gp_camera_trigger_capture",
        "gp_camera_file_set_info",
        "gp_camera_file_read",
    ];

    /// The source before its tests, with unix line endings.
    pub fn production(src: &str) -> String {
        let src = src.replace("\r\n", "\n");
        src[..src.find("#[cfg(test)]").unwrap()].to_string()
    }

    /// The function names inside the `dyn_api!` blocks: the only symbols ever looked up.
    pub fn declared(production: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = production;
        while let Some(i) = rest.find("dyn_api! {") {
            let body = &rest[i..];
            let end = body.find("\n    }\n").unwrap();
            out.extend(
                body[..end]
                    .lines()
                    .filter_map(|l| l.trim().strip_prefix("pub fn "))
                    .map(|l| l.split('(').next().unwrap().to_string()),
            );
            rest = &body[end..];
        }
        out
    }

    /// `src` (a phone module's source) only looks up the `allowed` functions, never names a
    /// writing one, never opens a library or looks up a symbol except through `dyn_api!`, and
    /// opens a file only for reading.
    pub fn assert_read_only(src: &str, allowed: &[&str]) {
        let prod = production(src);
        let declared = declared(&prod);
        assert_eq!(declared.len(), allowed.len(), "{declared:?}");
        for name in &declared {
            assert!(
                allowed.contains(&name.as_str()),
                "{name} is not an allowed function"
            );
        }
        for bad in FORBIDDEN {
            assert!(!prod.contains(bad), "{bad} must not appear");
        }
        for token in prod.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if token.starts_with("AFC_FOPEN_") {
                assert_eq!(token, "AFC_FOPEN_RDONLY", "only read-only opens");
            }
        }
        // Anything called that looks like one of these libraries' functions is in the allow-list.
        for (i, _) in prod.match_indices('(') {
            let before = &prod[..i];
            let start = before
                .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .map_or(0, |p| p + 1);
            let name = &before[start..];
            let c_like = ["LIBMTP_", "lockdownd_", "idevice_", "gp_"]
                .iter()
                .any(|p| name.starts_with(p));
            if c_like {
                assert!(allowed.contains(&name), "{name} is not an allowed function");
            }
        }
        for direct in ["dlsym", "dlopen", ".sym(", "Lib::open", "extern \"C\" {"] {
            assert!(!prod.contains(direct), "{direct} belongs in dynlib.rs only");
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use std::ffi::c_int;

    dyn_api! {
        sonames: &["libc.so.6"],
        off: "test",
        api: LibcApi,
        get: libc_api,
        load: load_libc_api;
        pub fn getpid() -> c_int;
    }

    mod with_a_missing_function {
        use std::ffi::c_int;

        dyn_api! {
            sonames: &["libc.so.6"],
            off: "test",
            api: MissingApi,
            get: missing_api,
            load: load_missing_api;
            pub fn getpid() -> c_int;
            pub fn pnk_no_such_function_here();
        }
    }
    use with_a_missing_function::{load_missing_api, missing_api};

    #[test]
    fn a_library_that_is_there_loads_and_its_functions_work() {
        if super::hidden() {
            return;
        }
        let api = load_libc_api(&["libc.so.6"]).unwrap();
        // SAFETY: getpid takes nothing.
        assert!(unsafe { (api.getpid)() } > 0);
    }

    #[test]
    fn a_missing_library_is_an_error_not_a_crash() {
        assert!(load_libc_api(&["libpnk-no-such-library.so.99", "libpnk-nor-this.so"]).is_err());
        // The macro's cached getter just says "unavailable" when the library can't be opened.
        let _ = (libc_api(), missing_api());
    }

    #[test]
    fn a_library_missing_one_function_is_unavailable_as_a_whole() {
        if super::hidden() {
            return;
        }
        let e = load_missing_api(&["libc.so.6"]).err().unwrap();
        assert!(e.contains("pnk_no_such_function_here"), "{e}");
        assert!(missing_api().is_none());
    }

    #[test]
    fn the_allow_list_check_rejects_a_writing_function() {
        let bad = "mod ffi {\n    dyn_api! {\n        pub fn LIBMTP_Send_File();\n    }\n}\n";
        assert!(std::panic::catch_unwind(|| super::check::assert_read_only(bad, &[])).is_err());
        let bad = "fn f() { let m = AFC_FOPEN_WRONLY; }\n";
        assert!(std::panic::catch_unwind(|| super::check::assert_read_only(bad, &[])).is_err());
        let bad = "fn f() { libc::dlsym(a, b); }\n";
        assert!(std::panic::catch_unwind(|| super::check::assert_read_only(bad, &[])).is_err());
        // Every camera function that writes is refused, declared or merely called.
        for name in super::check::FORBIDDEN
            .iter()
            .filter(|n| n.starts_with("gp_"))
        {
            let declared =
                format!("mod ffi {{\n    dyn_api! {{\n        pub fn {name}();\n    }}\n}}\n");
            assert!(
                std::panic::catch_unwind(|| super::check::assert_read_only(&declared, &[]))
                    .is_err(),
                "{name}"
            );
            let called = format!("fn f() {{ {name}(1); }}\n");
            assert!(
                std::panic::catch_unwind(|| super::check::assert_read_only(&called, &[])).is_err(),
                "{name} called"
            );
        }
        // An unknown gp_ function is not allowed either, even though it is not on the list.
        let unknown = "fn f() { gp_camera_do_something(1); }\n";
        assert!(std::panic::catch_unwind(|| super::check::assert_read_only(unknown, &[])).is_err());
    }
}
