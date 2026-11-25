//! Puts the Windows resources into the exe: the file icon Explorer shows, the application
//! manifest and the version info. (The window's own icon is set at run time from
//! assets/icon.png.) The version comes from the crate version, through a generated
//! `pnk-version.h` in OUT_DIR that assets/app.rc includes.
//!
//! assets/app.rc is compiled with `windres` on the GNU toolchain, or `rc.exe` on MSVC; set
//! `WINDRES` or `RC` to use a particular one. If neither is found, the build still succeeds,
//! with a warning, and the exe just has no file icon.
#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/plug-n-keep.exe.manifest");
    println!("cargo:rerun-if-env-changed=WINDRES");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let assets =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo")).join("assets");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("set by cargo"));
    let rc = assets.join("app.rc");
    write_version_header(&out_dir);

    let compiled = if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let out = out_dir.join("app.res");
        let tool = env::var("RC").unwrap_or_else(|_| "rc".into());
        Command::new(&tool)
            .arg("/nologo")
            .arg("/i")
            .arg(&assets)
            .arg("/i")
            .arg(&out_dir)
            .arg("/fo")
            .arg(&out)
            .arg(&rc)
            .status()
            .map(|s| s.success().then_some(out))
            .map_err(|e| format!("{tool}: {e}"))
    } else {
        let out = out_dir.join("app-res.o");
        let tool = env::var("WINDRES").unwrap_or_else(|_| "windres".into());
        Command::new(&tool)
            .arg("--include-dir")
            .arg(&assets)
            .arg("--include-dir")
            .arg(&out_dir)
            .arg("--input")
            .arg(&rc)
            .arg("--output")
            .arg(&out)
            .arg("--output-format=coff")
            .status()
            .map(|s| s.success().then_some(out))
            .map_err(|e| format!("{tool}: {e}"))
    };

    match compiled {
        // Passed straight to the linker (not as a library), so the resources can't be dropped.
        Ok(Some(object)) => {
            println!("cargo:rustc-link-arg-bins={}", object.display());
            // The manifest is in the resources; keep the MSVC linker from adding its own.
            if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
                println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
            }
        }
        Ok(None) => println!(
            "cargo:warning=the resource compiler failed; the exe gets no icon, manifest or version info"
        ),
        Err(e) => println!(
            "cargo:warning=no resource compiler ({e}); the exe gets no icon, manifest or version info"
        ),
    }
}

/// Writes `pnk-version.h` for app.rc from the crate version, so the version info never goes
/// stale. The numeric form is `major,minor,patch,0`; a pre-release suffix is left out of it.
fn write_version_header(out_dir: &std::path::Path) {
    let part = |name: &str| env::var(name).expect("set by cargo");
    let version = part("CARGO_PKG_VERSION");
    let header = format!(
        "#define PNK_VERSION_NUM {},{},{},0\r\n#define PNK_VERSION_STR \"{}\"\r\n",
        part("CARGO_PKG_VERSION_MAJOR"),
        part("CARGO_PKG_VERSION_MINOR"),
        part("CARGO_PKG_VERSION_PATCH"),
        version
    );
    std::fs::write(out_dir.join("pnk-version.h"), header).expect("write pnk-version.h");
}
