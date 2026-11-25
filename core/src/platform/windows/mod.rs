//! Windows: the file system through Win32, the settings folder in `%APPDATA%`, and portable
//! devices through WPD.

pub mod eject;
pub mod fs;
pub mod heic;
pub mod notify;
pub mod portable;
pub mod trash;
pub mod volume;

use std::path::PathBuf;

use crate::device::{DeviceInfo, DeviceSource};

use super::{ListedDevice, Transport};

pub use fs::{
    acquire_lock, disk_space, flush_dir, flush_file, is_link, local_stamp, local_time_ms,
    rename_no_replace, rename_replace, utc_stamp, volume_token,
};
pub use trash::send_to_bin;

/// `%APPDATA%\<app_dir>`.
pub fn settings_dir(app_dir: &str) -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(app_dir))
}

/// A WPD device as a listed one: the label is the name and maker, the way the picker shows it.
fn listed(d: &DeviceInfo) -> ListedDevice {
    let name = if d.friendly_name.is_empty() {
        &d.description
    } else {
        &d.friendly_name
    };
    let label = if d.manufacturer.is_empty() {
        name.clone()
    } else {
        format!("{name} ({})", d.manufacturer)
    };
    ListedDevice {
        id: d.pnp_id.clone(),
        label,
        kind_hint: portable::kind_hint_from_listing(d),
        transport: Transport::Portable,
    }
}

/// What the device thread needs from the system for as long as it runs: here, COM.
pub struct Session {
    com: portable::ComGuard,
}

impl Session {
    pub fn init() -> Result<Self, String> {
        Ok(Self {
            com: portable::ComGuard::init()?,
        })
    }

    /// The WPD devices that are plugged in, as listed devices.
    pub fn list_portable(&self) -> Result<Vec<ListedDevice>, String> {
        Ok(portable::list_devices(&self.com)?
            .iter()
            .map(listed)
            .collect())
    }

    pub fn open_portable(&self, pnp_id: &str) -> Result<Box<dyn DeviceSource>, String> {
        Ok(Box::new(portable::WpdSource::open(&self.com, pnp_id)?))
    }
}
