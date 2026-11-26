//! Plug-n-Keep: backs up a device to a folder on this PC and keeps everything that was ever on it.
//! A device is only ever read; the one exception is the small ID file you agree to write to a stick
//! (see core/src/idfile.rs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod app;
mod format;
mod log_view;
mod logging;
mod questions;
mod queue;
mod s3;
mod theme;
mod validate;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use pnk_core::state::{InstanceLock, StateDir, default_state_dir};

const TITLE: &str = "Plug-n-Keep";

/// Command-line options. Every build accepts `--state-dir`; only debug builds with the
/// `fake-device` feature accept the rest.
#[derive(Debug, Default)]
pub struct Options {
    /// Serve this folder as if it were a phone.
    pub fake_device: Option<PathBuf>,
    /// Read the fake device slowly (about 3 MB/s).
    pub fake_slow: bool,
    /// Make the fake device fall one file behind after a transfer is cut short, as an iPhone did.
    pub fake_lag: bool,
    /// Name the fake device after its folder (for screenshots).
    pub fake_named: bool,
    /// Use this folder instead of the usual settings folder (Local mode's state; S3 mode's is
    /// the `s3` folder inside it).
    pub state_dir: Option<PathBuf>,
}

/// The command line, as `args` (without the program name). `--state-dir <folder>` is always
/// accepted; the `--fake-*` options only when `with_fake` (debug builds with the `fake-device`
/// feature). Anything else is an error.
fn parse_options(
    args: impl IntoIterator<Item = std::ffi::OsString>,
    with_fake: bool,
) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--state-dir") => {
                opts.state_dir = Some(args.next().ok_or("--state-dir needs a folder")?.into())
            }
            Some("--fake-device") if with_fake => {
                opts.fake_device = Some(args.next().ok_or("--fake-device needs a folder")?.into())
            }
            Some("--fake-slow") if with_fake => opts.fake_slow = true,
            Some("--fake-lag") if with_fake => opts.fake_lag = true,
            Some("--fake-named") if with_fake => opts.fake_named = true,
            _ => return Err(format!("Unknown option: {}", arg.to_string_lossy())),
        }
    }
    Ok(opts)
}

fn parse_args() -> Result<Options, String> {
    parse_options(
        std::env::args_os().skip(1),
        cfg!(all(debug_assertions, feature = "fake-device")),
    )
}

/// Show an error box and exit. There is no console in release builds.
fn fail(msg: &str) -> ! {
    log::error!("{msg}");
    eprintln!("{msg}");
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(TITLE)
        .set_description(msg)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    std::process::exit(1);
}

/// Without a GPU, Wayland can fail to open a window while X11 (through Xwayland) works. Start this
/// program again with Wayland hidden, once. Returns if it can't, or already tried.
#[cfg(all(unix, not(target_os = "macos")))]
fn retry_on_x11(lock: InstanceLock) {
    use std::os::unix::process::CommandExt;
    const MARKER: &str = "PNK_X11_RETRY";
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if !set("WAYLAND_DISPLAY") || !set("DISPLAY") || std::env::var_os(MARKER).is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    log::warn!("Trying again on X11");
    // The new process must be able to take the lock; dropping closes the locked file.
    drop(lock);
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env_remove("WAYLAND_DISPLAY")
        .env(MARKER, "1")
        .exec();
    log::error!("Could not start again: {err}");
}

fn main() {
    // First, so everything after it is logged. The saved level is applied once it is read.
    let logger = logging::install(log::LevelFilter::Info);
    log::info!("Plug-n-Keep {} is starting", env!("CARGO_PKG_VERSION"));
    let opts = parse_args().unwrap_or_else(|e| fail(&e));
    let dir = opts
        .state_dir
        .clone()
        .or_else(default_state_dir)
        .unwrap_or_else(|| {
            fail("Could not find the settings folder (HOME or APPDATA is not set).")
        });
    let state = StateDir::open(&dir).unwrap_or_else(|e| {
        fail(&format!(
            "Could not open the settings folder {}: {e}",
            dir.display()
        ))
    });
    let s3_state = StateDir::open(pnk_core::modes::s3_dir(&dir)).unwrap_or_else(|e| {
        fail(&format!(
            "Could not open the S3 settings folder inside {}: {e}",
            dir.display()
        ))
    });
    // Held until the window closes: a second copy would fight over the device and the files.
    let _lock = match InstanceLock::acquire(&state) {
        Ok(Some(lock)) => lock,
        Ok(None) => fail("Plug-n-Keep is already running."),
        Err(e) => fail(&format!("Could not check for another running copy: {e}")),
    };
    logger.set_level(state.config().log_level.filter());
    log::info!("Settings folder: {}", state.path().display());
    if let Some(folder) = &opts.fake_device {
        log::info!(
            "Fake-device mode: {} stands in for a phone (slow {}, lag {})",
            folder.display(),
            opts.fake_slow,
            opts.fake_lag
        );
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(TITLE)
        // Small enough to fit a 1280x720 (logical) screen with its taskbar.
        .with_inner_size([1120.0, 620.0])
        .with_min_inner_size([760.0, 440.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };
    let result = eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, state, s3_state, opts, logger)))),
    );
    if let Err(e) = result {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            log::warn!("Could not open the window: {e}");
            retry_on_x11(_lock);
            fail(&format!(
                "Could not open the window: {e}
The graphics driver couldn't start; running                  `env -u WAYLAND_DISPLAY plug-n-keep` may help."
            ));
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        fail(&format!("Could not open the window: {e}"));
    }
    log::info!("The window was closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<std::ffi::OsString> {
        a.iter().map(Into::into).collect()
    }

    #[test]
    fn state_dir_is_accepted_in_every_build_and_the_rest_only_with_fake_device() {
        for with_fake in [false, true] {
            let o = parse_options(args(&["--state-dir", "C:/x"]), with_fake).unwrap();
            assert_eq!(o.state_dir, Some(PathBuf::from("C:/x")));
            assert!(parse_options(args(&["--state-dir"]), with_fake).is_err());
            let e = parse_options(args(&["--bogus"]), with_fake).unwrap_err();
            assert!(e.starts_with("Unknown option: --bogus"), "{e}");
        }
        assert!(parse_options(args(&[]), false).unwrap().state_dir.is_none());
        // The fake-device options are refused in a release build, taken in a debug one.
        for o in ["--fake-slow", "--fake-lag", "--fake-named"] {
            assert!(parse_options(args(&[o]), false).is_err(), "{o}");
            assert!(parse_options(args(&[o]), true).is_ok(), "{o}");
        }
        assert!(parse_options(args(&["--fake-device", "C:/p"]), false).is_err());
        let o = parse_options(
            args(&["--fake-device", "C:/p", "--state-dir", "D:/s"]),
            true,
        )
        .unwrap();
        assert_eq!(o.fake_device, Some(PathBuf::from("C:/p")));
    }
}
