//! Plug-in and removal detection on Windows: `CM_Register_Notification` for device interfaces,
//! with no window and no message loop. It is registered for the two interfaces a backup source
//! shows up as: portable devices (WPD) and volumes. The system calls back on one of its own
//! threads, which only sends a message down a channel; a thread of ours collects the messages,
//! waits for a short quiet period and wakes the caller.
//!
//! The watcher only says that something changed; the window lists the devices.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_NOTIFY_ACTION, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0,
    CM_NOTIFY_FILTER_0_2, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification,
    CM_Unregister_Notification, CR_SUCCESS, HCMNOTIFICATION,
};
use windows::core::GUID;

use crate::platform::poll::{Debounce, QUIET, Wake, Watcher};

/// `GUID_DEVINTERFACE_VOLUME`: a volume (drive letter) appears or goes.
const VOLUME: GUID = GUID::from_u128(0x53f5630d_b6bf_11d0_94f2_00a0c91efb8b);
/// `GUID_DEVINTERFACE_WPD`: a portable device (phone, camera) appears or goes.
const WPD: GUID = GUID::from_u128(0x6ac27878_a6fa_4155_ba85_f98f491d4f33);

/// `CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL` and `...REMOVAL`.
const ARRIVAL: i32 = 0;
const REMOVAL: i32 = 1;

/// What the system said happened to a device interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plug {
    Arrived,
    Removed,
}

/// Reads the action number the system passes to the callback. Only arrivals and removals of an
/// interface count; anything else (a query about removing it, say) gives `None`.
pub fn plug_from_action(action: i32) -> Option<Plug> {
    match action {
        ARRIVAL => Some(Plug::Arrived),
        REMOVAL => Some(Plug::Removed),
        _ => None,
    }
}

/// What the callback needs: where to send.
struct Context {
    tx: Mutex<Sender<Plug>>,
}

/// Called by the system, on one of its threads: sends the event on and returns at once.
unsafe extern "system" fn on_event(
    _handle: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    if context.is_null() {
        return 0;
    }
    // SAFETY: the context is the `Context` made in `watch`, which is only freed after the
    // registrations have been removed (and removing one waits for callbacks in progress).
    let ctx = unsafe { &*context.cast::<Context>() };
    if let Some(p) = plug_from_action(action.0)
        && let Ok(tx) = ctx.tx.lock()
    {
        let _ = tx.send(p);
    }
    0
}

/// One registration for one interface class, removed when dropped.
struct Registration {
    handle: HCMNOTIFICATION,
}

// SAFETY: the handle is only used to unregister, which the system allows from any thread.
unsafe impl Send for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful registration and is unregistered once.
        let _ = unsafe { CM_Unregister_Notification(self.handle) };
    }
}

fn register(class: GUID, context: *const Context) -> Result<Registration, String> {
    let mut filter = CM_NOTIFY_FILTER {
        cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
        FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
        ..Default::default()
    };
    filter.u = CM_NOTIFY_FILTER_0 {
        DeviceInterface: CM_NOTIFY_FILTER_0_2 { ClassGuid: class },
    };
    let mut handle = HCMNOTIFICATION::default();
    // SAFETY: the filter and the out-pointer are live locals; the context outlives the
    // registration (see `watch`).
    let rc = unsafe {
        CM_Register_Notification(&filter, Some(context.cast()), Some(on_event), &mut handle)
    };
    if rc == CR_SUCCESS {
        Ok(Registration { handle })
    } else {
        Err(format!(
            "could not ask Windows for device notifications (error {})",
            rc.0
        ))
    }
}

/// Starts watching. `wake` is called, from another thread, when the devices may have changed.
pub fn watch(wake: Wake) -> Result<Watcher, String> {
    let (tx, rx) = mpsc::channel::<Plug>();
    let context = Box::into_raw(Box::new(Context { tx: Mutex::new(tx) }));
    let registrations = [VOLUME, WPD]
        .into_iter()
        .map(|class| register(class, context))
        .collect::<Result<Vec<_>, _>>();
    let registrations = match registrations {
        Ok(r) => r,
        Err(e) => {
            // SAFETY: no registration is left, so nothing can call back with the context.
            drop(unsafe { Box::from_raw(context) });
            return Err(e);
        }
    };
    info!("Watching for plugged-in devices through Windows device notifications");
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let spawned = std::thread::Builder::new()
        .name("device-watch".into())
        .spawn(move || {
            let mut debounce = Debounce::default();
            while !flag.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(p) => {
                        debug!("Device interface event: {p:?}");
                        debounce.event(Instant::now());
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if debounce.due(Instant::now(), QUIET) {
                    wake();
                }
            }
        });
    if let Err(e) = spawned {
        warn!("Could not start the device watcher: {e}");
        drop(registrations);
        // SAFETY: the registrations are gone, so the context is no longer in use.
        drop(unsafe { Box::from_raw(context) });
        return Err(format!("could not start the device watcher: {e}"));
    }
    let addr = context as usize;
    Ok(Watcher::new(
        stop,
        Some(Box::new(move || {
            // Unregistering waits for any callback in progress, so the context can go after it.
            drop(registrations);
            // SAFETY: `addr` is the context made above, freed here exactly once.
            drop(unsafe { Box::from_raw(addr as *mut Context) });
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_arrivals_and_removals_of_an_interface_count() {
        assert_eq!(plug_from_action(0), Some(Plug::Arrived));
        assert_eq!(plug_from_action(1), Some(Plug::Removed));
        for other in [2, 3, 4, -1, 100] {
            assert_eq!(plug_from_action(other), None, "{other}");
        }
    }

    /// Registering and dropping works and wakes nothing by itself.
    #[test]
    fn a_watcher_starts_and_stops() {
        let w = watch(Box::new(|| {})).expect("register for notifications");
        drop(w);
    }
}
