//! Plug-in and removal detection on FreeBSD: a slow poll of the mounted volumes (a `devd`
//! listener comes later). The poll only says something changed; the window lists the devices.

use std::time::Duration;

use super::volume;
use crate::platform::poll::{Wake, Watcher, spawn_poll};

/// How often the mounted volumes are looked at.
const EVERY: Duration = Duration::from_secs(3);

/// What the poll compares: the identifiers of the volumes that are listed.
fn snapshot() -> Vec<String> {
    volume::list().into_iter().map(|d| d.id).collect()
}

/// Starts watching. `wake` is called, from another thread, when the devices may have changed.
pub fn watch(wake: Wake) -> Result<Watcher, String> {
    spawn_poll("device-watch", EVERY, snapshot, wake)
}
