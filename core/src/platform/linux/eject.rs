//! Eject for a stick or drive on Linux, through udisks2 over D-Bus (the service the desktop's
//! own Eject uses). Nothing is opened, written or locked by the app.
//!
//! The mount point of the listed volume finds its block device in udisks, the block device its
//! drive, and the drive every mounted partition it has. Each of those partitions is unmounted
//! (`Filesystem.Unmount`, with no options, so never forced), then the drive is powered off
//! (`Drive.PowerOff`), or ejected (`Drive.Eject`) only where it can't be powered off. If the
//! power-off or the eject fails after the unmounts worked, the drive is still safe to unplug,
//! and that is what the user is told.
//!
//! **Refusals.** Nothing is unmounted or powered off when:
//! - the drive is not a removable or USB one;
//! - one of its partitions is mounted at `/`, `/boot`, `/usr`, `/home` or another of the system's
//!   own mount points (a live USB:
//!   powering it off would take the running system away);
//! - an encrypted (LUKS) or LVM layer on it is still open and mounted. Powering the drive off
//!   under a mounted volume would cut it off mid-write, and locking it from here is not this
//!   app's job, so the user is asked to close it first. The check follows `CryptoBackingDevice`
//!   from every block back to the drive; for LVM, where udisks does not tie a logical volume to
//!   its disk as plainly, any mounted device-mapper volume counts when a partition of the drive
//!   is an LVM physical volume (a refusal that may be too careful, never too bold).
//!
//! Every call to udisks is given 30 seconds, so a hung udisks2 can't hang the worker.
//!
//! The D-Bus calls sit behind [`Udisks`], so the mapping from a mount to the udisks objects and
//! the wording of each failure are tested without a device or a bus.

use std::collections::HashMap;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::platform::ListedDevice;

const SERVICE: &str = "org.freedesktop.UDisks2";
const ROOT: &str = "/org/freedesktop/UDisks2";

/// A block device udisks knows: a disk or a partition.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Block {
    /// Its udisks object path.
    pub path: String,
    /// Its device node, `/dev/sdb1`.
    pub device: String,
    /// The drive it belongs to (an object path), if it has one.
    pub drive: Option<String>,
    /// Where it is mounted now. Empty when it isn't.
    pub mount_points: Vec<String>,
    /// For an unlocked encrypted volume: the block it is stored in (`CryptoBackingDevice`).
    pub crypto_backing: Option<String>,
    /// Whether this block is an LVM physical volume.
    pub lvm_pv: bool,
}

/// A drive udisks knows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Drive {
    pub path: String,
    pub removable: bool,
    /// `usb`, `ata`, `sdio` and so on, if udisks says.
    pub bus: String,
    pub can_power_off: bool,
    pub ejectable: bool,
}

/// Everything udisks lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Objects {
    pub blocks: Vec<Block>,
    pub drives: Vec<Drive>,
}

/// What to do for one eject: the partitions to unmount, then the drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub drive: Drive,
    /// Object paths of the mounted partitions of the drive, in a fixed order.
    pub unmount: Vec<String>,
}

/// From a listed volume's mount point (and its device node, as a second way to find it) to what
/// udisks must be asked. The drive must be a removable one or on USB: this never powers off a
/// fixed internal disk.
pub fn plan(objects: &Objects, mount_point: &str, dev_node: Option<&str>) -> Result<Plan, String> {
    let block = objects
        .blocks
        .iter()
        .find(|b| b.mount_points.iter().any(|m| m == mount_point))
        .or_else(|| {
            dev_node.and_then(|d| {
                objects
                    .blocks
                    .iter()
                    .find(|b| !b.device.is_empty() && b.device == d)
            })
        })
        .ok_or_else(|| {
            "The system's disk service (udisks2) doesn't know this drive, so it can't be ejected \
             from here. Use the desktop's own Eject."
                .to_string()
        })?;
    let drive_path = block.drive.as_deref().ok_or_else(|| {
        "The system doesn't say which drive this is on, so it can't be ejected from here."
            .to_string()
    })?;
    let drive = objects
        .drives
        .iter()
        .find(|d| d.path == drive_path)
        .ok_or_else(|| "The system doesn't know the drive this is on.".to_string())?;
    if !drive.removable && drive.bus != "usb" {
        return Err(
            "The system says this is not a removable drive, so it is not ejected from here.".into(),
        );
    }
    let mut unmount: Vec<String> = objects
        .blocks
        .iter()
        .filter(|b| b.drive.as_deref() == Some(drive_path) && !b.mount_points.is_empty())
        .map(|b| b.path.clone())
        .collect();
    unmount.sort();
    unmount.dedup();
    let own: Vec<&Block> = objects
        .blocks
        .iter()
        .filter(|b| b.drive.as_deref() == Some(drive_path))
        .collect();
    // A live USB: the running system is on it.
    if let Some(m) = own
        .iter()
        .flat_map(|b| &b.mount_points)
        .find(|m| is_system_mount(m))
    {
        return Err(format!(
            "This drive holds part of the running system (mounted at {m}), so it can't be \
             ejected from here."
        ));
    }
    // Encrypted or LVM volumes still open on top of it.
    let mut covered: Vec<&str> = own.iter().map(|b| b.path.as_str()).collect();
    loop {
        let more: Vec<&str> = objects
            .blocks
            .iter()
            .filter(|b| !covered.contains(&b.path.as_str()))
            .filter(|b| {
                b.crypto_backing
                    .as_deref()
                    .is_some_and(|c| covered.contains(&c))
            })
            .map(|b| b.path.as_str())
            .collect();
        if more.is_empty() {
            break;
        }
        covered.extend(more);
    }
    let has_pv = own.iter().any(|b| b.lvm_pv);
    let open: Vec<&Block> = objects
        .blocks
        .iter()
        .filter(|b| b.drive.as_deref() != Some(drive_path) && !b.mount_points.is_empty())
        .filter(|b| {
            covered.contains(&b.path.as_str()) || (has_pv && b.device.starts_with("/dev/dm-"))
        })
        .collect();
    if let Some(b) = open.first() {
        return Err(format!(
            "An encrypted or LVM part of this drive is still open ({}), so it is not ejected. \
             Close it first (unmount it and lock it), then try again.",
            if b.device.is_empty() {
                &b.path
            } else {
                &b.device
            }
        ));
    }
    Ok(Plan {
        drive: drive.clone(),
        unmount,
    })
}

/// Whether a mount point is one of the running system's own (`/`, `/boot`, `/boot/efi`, `/usr`,
/// `/home`, `/var` and so on: the same list the volume list never offers, by whole mount point).
/// A stick mounted somewhere under `/home` is an ordinary stick.
fn is_system_mount(m: &str) -> bool {
    super::volume::SYSTEM_MOUNTS.contains(&m)
}

/// Why a call to udisks failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UError {
    /// No udisks2 on the bus.
    NotRunning,
    /// The partition is in use.
    Busy,
    /// The system asked for rights this account doesn't have (or a password that wasn't given).
    NotAllowed,
    /// It wasn't mounted: nothing to undo.
    NotMounted,
    Other(String),
}

/// Sorts a D-Bus error by its name (or, for errors that aren't replies, its debug text).
pub fn classify(code: &str, text: &str) -> UError {
    let has = |s: &str| code.contains(s);
    if has("ServiceUnknown") || has("NameHasNoOwner") {
        UError::NotRunning
    } else if has("DeviceBusy") {
        UError::Busy
    } else if has("NotAuthorized") {
        UError::NotAllowed
    } else if has("NotMounted") {
        UError::NotMounted
    } else {
        UError::Other(if text.is_empty() {
            code.to_string()
        } else {
            text.to_string()
        })
    }
}

/// The udisks2 calls, as the eject needs them.
pub trait Udisks {
    fn objects(&self) -> Result<Objects, UError>;
    fn unmount(&self, block: &str) -> Result<(), UError>;
    fn power_off(&self, drive: &str) -> Result<(), UError>;
    fn eject(&self, drive: &str) -> Result<(), UError>;
}

fn describe(e: &UError, what: &str) -> String {
    match e {
        UError::NotRunning => "The system's disk service (udisks2) isn't running, so the drive \
             can't be ejected from here. Start it (Ubuntu: sudo apt install udisks2), or use \
             the desktop's own Eject."
            .into(),
        UError::Busy => format!(
            "{what} is still in use. Close any file manager window, terminal or program that \
             uses it, and try again."
        ),
        UError::NotAllowed => format!(
            "This account isn't allowed to eject {what}. Use the desktop's own Eject, which can \
             ask for a password."
        ),
        UError::NotMounted => format!("{what} was not mounted."),
        UError::Other(t) => format!("Could not eject {what}: {t}"),
    }
}

/// Ejects the volume mounted at `mount_point` through `udisks`; `what` is how the user's text
/// names it.
pub fn eject_with(
    udisks: &dyn Udisks,
    mount_point: &str,
    dev_node: Option<&str>,
    what: &str,
) -> Result<(), String> {
    let objects = udisks.objects().map_err(|e| describe(&e, what))?;
    let plan = plan(&objects, mount_point, dev_node)?;
    log::info!(
        "Ejecting {what}: unmounting {}, then powering off {}",
        plan.unmount.len(),
        plan.drive.path
    );
    // The device names of what is unmounted so far, for the message if a later one fails.
    let mut unmounted: Vec<String> = Vec::new();
    let name_of = |block: &str| {
        objects
            .blocks
            .iter()
            .find(|b| b.path == block)
            .map_or_else(|| block.to_string(), |b| b.device.clone())
    };
    for block in &plan.unmount {
        match udisks.unmount(block) {
            Ok(()) | Err(UError::NotMounted) => unmounted.push(name_of(block)),
            Err(e) => {
                log::warn!("Could not unmount {block}: {e:?}");
                let mut text = describe(&e, what);
                if !unmounted.is_empty() {
                    text += &format!(
                        " Already unmounted: {}. The rest of the drive is still mounted.",
                        unmounted.join(", ")
                    );
                }
                return Err(text);
            }
        }
    }
    // From here the data is flushed and the file systems are unmounted: the drive is safe to
    // unplug whatever happens next.
    let stop = if plan.drive.can_power_off {
        Some(udisks.power_off(&plan.drive.path))
    } else if plan.drive.ejectable {
        Some(udisks.eject(&plan.drive.path))
    } else {
        None
    };
    match stop {
        Some(Err(e)) => log::warn!(
            "Unmounted {what}, but the drive could not be powered off ({e:?}); it is safe to unplug"
        ),
        Some(Ok(())) => {}
        None => {
            log::info!("Unmounted {what}; the drive can't be powered off, it is safe to unplug")
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The real bus
// ---------------------------------------------------------------------------------------

struct Bus(Connection);

/// How long one call to udisks may take.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Runs `f` on a helper thread and waits `limit` for its answer. A call that hangs is given up on
/// (its thread is left to finish or hang by itself) and reported as an error.
fn timed<T: Send + 'static>(
    limit: std::time::Duration,
    f: impl FnOnce() -> Result<T, UError> + Send + 'static,
) -> Result<T, UError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).unwrap_or_else(|_| {
        Err(UError::Other(format!(
            "udisks2 did not answer within {} seconds",
            limit.as_secs().max(1)
        )))
    })
}

fn to_error(e: zbus::Error) -> UError {
    match &e {
        zbus::Error::MethodError(name, text, _) => {
            classify(name.as_str(), text.as_deref().unwrap_or_default())
        }
        other => classify(&format!("{other:?}"), &other.to_string()),
    }
}

fn bytes_of(v: &Value<'_>) -> Option<Vec<u8>> {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::U8(b) => Some(*b),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// A NUL-ended byte string, as udisks sends device nodes and mount points.
fn text_of(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

type Props = HashMap<String, OwnedValue>;

fn parse_objects(all: HashMap<OwnedObjectPath, HashMap<String, Props>>) -> Objects {
    let mut out = Objects::default();
    for (path, ifaces) in all {
        let path = path.as_str().to_string();
        if let Some(b) = ifaces.get("org.freedesktop.UDisks2.Block") {
            let drive = match b.get("Drive").map(|v| &**v) {
                Some(Value::ObjectPath(p)) if p.as_str() != "/" => Some(p.as_str().to_string()),
                _ => None,
            };
            let device = b
                .get("Device")
                .and_then(|v| bytes_of(v))
                .map(|b| text_of(&b))
                .unwrap_or_default();
            let mount_points = ifaces
                .get("org.freedesktop.UDisks2.Filesystem")
                .and_then(|f| f.get("MountPoints"))
                .map(|v| match &**v {
                    Value::Array(a) => a
                        .iter()
                        .filter_map(|x| bytes_of(x).map(|b| text_of(&b)))
                        .collect(),
                    _ => Vec::new(),
                })
                .unwrap_or_default();
            let crypto_backing = match b.get("CryptoBackingDevice").map(|v| &**v) {
                Some(Value::ObjectPath(p)) if p.as_str() != "/" => Some(p.as_str().to_string()),
                _ => None,
            };
            out.blocks.push(Block {
                path: path.clone(),
                device,
                drive,
                mount_points,
                crypto_backing,
                lvm_pv: ifaces.contains_key("org.freedesktop.UDisks2.PhysicalVolume"),
            });
        }
        if let Some(d) = ifaces.get("org.freedesktop.UDisks2.Drive") {
            let flag = |name: &str| matches!(d.get(name).map(|v| &**v), Some(Value::Bool(true)));
            out.drives.push(Drive {
                path,
                removable: flag("Removable"),
                bus: match d.get("ConnectionBus").map(|v| &**v) {
                    Some(Value::Str(s)) => s.to_string(),
                    _ => String::new(),
                },
                can_power_off: flag("CanPowerOff"),
                ejectable: flag("Ejectable"),
            });
        }
    }
    out
}

impl Bus {
    fn call(&self, path: &str, interface: &str, method: &str) -> Result<(), UError> {
        let conn = self.0.clone();
        let (path, interface, method) =
            (path.to_string(), interface.to_string(), method.to_string());
        timed(CALL_TIMEOUT, move || {
            let proxy =
                Proxy::new(&conn, SERVICE, path.as_str(), interface.as_str()).map_err(to_error)?;
            // No options: nothing is forced.
            let options: HashMap<&str, Value<'_>> = HashMap::new();
            proxy
                .call::<_, _, ()>(method.as_str(), &(options,))
                .map_err(to_error)
        })
    }
}

impl Udisks for Bus {
    fn objects(&self) -> Result<Objects, UError> {
        let conn = self.0.clone();
        timed(CALL_TIMEOUT, move || {
            let proxy = Proxy::new(&conn, SERVICE, ROOT, "org.freedesktop.DBus.ObjectManager")
                .map_err(to_error)?;
            let all: HashMap<OwnedObjectPath, HashMap<String, Props>> =
                proxy.call("GetManagedObjects", &()).map_err(to_error)?;
            Ok(parse_objects(all))
        })
    }

    fn unmount(&self, block: &str) -> Result<(), UError> {
        self.call(block, "org.freedesktop.UDisks2.Filesystem", "Unmount")
    }

    fn power_off(&self, drive: &str) -> Result<(), UError> {
        self.call(drive, "org.freedesktop.UDisks2.Drive", "PowerOff")
    }

    fn eject(&self, drive: &str) -> Result<(), UError> {
        self.call(drive, "org.freedesktop.UDisks2.Drive", "Eject")
    }
}

/// Ejects the stick or drive `device` (its ID is the mount point).
pub fn eject(device: &ListedDevice) -> Result<(), String> {
    let conn = Connection::system().map_err(|e| {
        log::warn!("No connection to the system bus: {e}");
        "Could not reach the system's message bus (D-Bus), so the drive can't be ejected from \
         here. Use the desktop's own Eject."
            .to_string()
    })?;
    let dev_node = super::volume::device_of_mount(&device.id);
    eject_with(
        &Bus(conn),
        &device.id,
        dev_node.as_deref(),
        &format!("the drive at {}", device.id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn block(path: &str, device: &str, drive: Option<&str>, mounts: &[&str]) -> Block {
        Block {
            path: path.into(),
            device: device.into(),
            drive: drive.map(String::from),
            mount_points: mounts.iter().map(|s| s.to_string()).collect(),
            ..Block::default()
        }
    }

    fn drive(path: &str, removable: bool, bus: &str, power: bool, eject: bool) -> Drive {
        Drive {
            path: path.into(),
            removable,
            bus: bus.into(),
            can_power_off: power,
            ejectable: eject,
        }
    }

    const D: &str = "/org/freedesktop/UDisks2/drives/Stick_1";
    const D2: &str = "/org/freedesktop/UDisks2/drives/Other_2";
    const B1: &str = "/org/freedesktop/UDisks2/block_devices/sdb1";
    const B2: &str = "/org/freedesktop/UDisks2/block_devices/sdb2";

    fn world() -> Objects {
        Objects {
            blocks: vec![
                block(
                    "/org/freedesktop/UDisks2/block_devices/sda1",
                    "/dev/sda1",
                    Some(D2),
                    &["/"],
                ),
                block(
                    "/org/freedesktop/UDisks2/block_devices/sdb",
                    "/dev/sdb",
                    Some(D),
                    &[],
                ),
                block(B2, "/dev/sdb2", Some(D), &["/media/me/DATA"]),
                block(B1, "/dev/sdb1", Some(D), &["/media/me/STICK", "/mnt/also"]),
            ],
            drives: vec![
                drive(D2, false, "ata", false, false),
                drive(D, true, "usb", true, true),
            ],
        }
    }

    #[test]
    fn a_mount_point_maps_to_its_drive_and_every_mounted_partition_of_it() {
        let p = plan(&world(), "/media/me/STICK", None).unwrap();
        assert_eq!(p.drive.path, D);
        assert_eq!(p.unmount, [B1, B2], "both partitions, in path order");
        // Found from the other partition as well, and the same drive.
        assert_eq!(
            plan(&world(), "/media/me/DATA", None).unwrap().drive.path,
            D
        );
    }

    #[test]
    fn the_device_node_finds_it_when_the_mount_point_isnt_known() {
        let p = plan(&world(), "/somewhere/else", Some("/dev/sdb1")).unwrap();
        assert_eq!(p.drive.path, D);
        assert!(plan(&world(), "/somewhere/else", Some("/dev/sdz9")).is_err());
        assert!(plan(&world(), "/somewhere/else", None).is_err());
        // An empty device node never matches an empty one.
        assert!(plan(&world(), "/somewhere/else", Some("")).is_err());
    }

    #[test]
    fn a_fixed_internal_disk_is_never_ejected() {
        let e = plan(&world(), "/", Some("/dev/sda1")).unwrap_err();
        assert!(e.contains("not a removable drive"), "{e}");
    }

    #[test]
    fn a_volume_with_no_drive_is_refused() {
        let mut w = world();
        w.blocks
            .push(block("/b/loop0", "/dev/loop0", None, &["/mnt/loop"]));
        assert!(
            plan(&w, "/mnt/loop", None)
                .unwrap_err()
                .contains("which drive")
        );
        w.blocks
            .push(block("/b/x", "/dev/x", Some("/drives/gone"), &["/mnt/x"]));
        assert!(
            plan(&w, "/mnt/x", None)
                .unwrap_err()
                .contains("doesn't know the drive")
        );
    }

    #[test]
    fn a_drive_with_an_open_encrypted_volume_is_refused() {
        let mut w = world();
        // The stick's second partition is LUKS; its unlocked volume is mounted.
        let mut clear = block(
            "/org/freedesktop/UDisks2/block_devices/dm_2",
            "/dev/dm-2",
            None,
            &["/media/me/SECRET"],
        );
        clear.crypto_backing = Some(B2.into());
        w.blocks.push(clear);
        let e = plan(&w, "/media/me/STICK", None).unwrap_err();
        assert!(e.contains("encrypted") && e.contains("/dev/dm-2"), "{e}");
        // Unlocked but not mounted: nothing is in use, so it can go.
        w.blocks.last_mut().unwrap().mount_points.clear();
        assert!(plan(&w, "/media/me/STICK", None).is_ok());
        // Layers on layers (LUKS inside LUKS) are followed.
        let mut inner = block("/b/dm_3", "/dev/dm-3", None, &["/mnt/inner"]);
        inner.crypto_backing = Some("/org/freedesktop/UDisks2/block_devices/dm_2".into());
        w.blocks.push(inner);
        assert!(plan(&w, "/media/me/STICK", None).is_err());
        // An encrypted volume of another drive is no concern of this one.
        let mut other = block("/b/dm_9", "/dev/dm-9", None, &["/mnt/other"]);
        other.crypto_backing = Some("/org/freedesktop/UDisks2/block_devices/sda1".into());
        let mut w = world();
        w.blocks.push(other);
        assert!(plan(&w, "/media/me/STICK", None).is_ok());
    }

    #[test]
    fn a_drive_with_an_lvm_volume_mounted_is_refused() {
        let mut w = world();
        w.blocks.iter_mut().find(|b| b.path == B2).unwrap().lvm_pv = true;
        w.blocks
            .push(block("/b/dm_0", "/dev/dm-0", None, &["/mnt/lv"]));
        let e = plan(&w, "/media/me/STICK", None).unwrap_err();
        assert!(e.contains("LVM"), "{e}");
        // No physical volume on this drive: a mounted device-mapper volume is someone else's.
        let mut w = world();
        w.blocks
            .push(block("/b/dm_0", "/dev/dm-0", None, &["/mnt/lv"]));
        assert!(plan(&w, "/media/me/STICK", None).is_ok());
    }

    #[test]
    fn a_live_usb_is_never_ejected() {
        for mount in ["/", "/boot", "/boot/efi", "/usr", "/home", "/var"] {
            let mut w = world();
            w.blocks
                .iter_mut()
                .find(|b| b.path == B2)
                .unwrap()
                .mount_points
                .push(mount.into());
            let e = plan(&w, "/media/me/STICK", None).unwrap_err();
            assert!(e.contains("running system"), "{mount}: {e}");
        }
        // Names that only start alike, or a folder under /home, are not system mount points.
        let mut w = world();
        w.blocks
            .iter_mut()
            .find(|b| b.path == B2)
            .unwrap()
            .mount_points
            .push("/homework".into());
        assert!(plan(&w, "/media/me/STICK", None).is_ok());
        let mut w = world();
        w.blocks
            .iter_mut()
            .find(|b| b.path == B2)
            .unwrap()
            .mount_points
            .push("/home/me/stick".into());
        assert!(plan(&w, "/media/me/STICK", None).is_ok());
    }

    #[test]
    fn a_later_unmount_failing_names_what_is_already_unmounted() {
        let f = Fake {
            objects: world(),
            busy: Some(B2.into()),
            ..Fake::default()
        };
        let e = eject_with(&f, "/media/me/STICK", None, "the stick").unwrap_err();
        assert!(e.contains("Already unmounted: /dev/sdb1"), "{e}");
        // Nothing unmounted yet: nothing to add.
        let f = Fake {
            objects: world(),
            busy: Some(B1.into()),
            ..Fake::default()
        };
        let e = eject_with(&f, "/media/me/STICK", None, "the stick").unwrap_err();
        assert!(!e.contains("Already unmounted"), "{e}");
    }

    #[test]
    fn a_call_that_never_answers_is_given_up_on() {
        let r: Result<(), UError> = timed(std::time::Duration::from_millis(50), || {
            std::thread::sleep(std::time::Duration::from_secs(5));
            Ok(())
        });
        match r {
            Err(UError::Other(t)) => assert!(t.contains("did not answer"), "{t}"),
            other => panic!("{other:?}"),
        }
        let r = timed(std::time::Duration::from_secs(5), || Ok(7));
        assert_eq!(r, Ok(7));
    }

    #[test]
    fn errors_are_sorted_by_their_name() {
        assert_eq!(
            classify("org.freedesktop.DBus.Error.ServiceUnknown", ""),
            UError::NotRunning
        );
        assert_eq!(classify("NameHasNoOwner", ""), UError::NotRunning);
        assert_eq!(
            classify("org.freedesktop.UDisks2.Error.DeviceBusy", "x"),
            UError::Busy
        );
        assert_eq!(
            classify("org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain", ""),
            UError::NotAllowed
        );
        assert_eq!(
            classify("org.freedesktop.UDisks2.Error.NotMounted", ""),
            UError::NotMounted
        );
        assert_eq!(
            classify("org.freedesktop.UDisks2.Error.Failed", "boom"),
            UError::Other("boom".into())
        );
        assert!(describe(&UError::NotRunning, "x").contains("udisks2"));
        assert!(describe(&UError::Busy, "the drive").contains("still in use"));
    }

    #[derive(Default)]
    struct Fake {
        objects: Objects,
        calls: RefCell<Vec<String>>,
        busy: Option<String>,
        power_off_fails: bool,
        down: bool,
    }

    impl Udisks for Fake {
        fn objects(&self) -> Result<Objects, UError> {
            if self.down {
                return Err(UError::NotRunning);
            }
            Ok(self.objects.clone())
        }
        fn unmount(&self, block: &str) -> Result<(), UError> {
            self.calls.borrow_mut().push(format!("unmount {block}"));
            if self.busy.as_deref() == Some(block) {
                Err(UError::Busy)
            } else {
                Ok(())
            }
        }
        fn power_off(&self, drive: &str) -> Result<(), UError> {
            self.calls.borrow_mut().push(format!("power_off {drive}"));
            if self.power_off_fails {
                Err(UError::Other("no".into()))
            } else {
                Ok(())
            }
        }
        fn eject(&self, drive: &str) -> Result<(), UError> {
            self.calls.borrow_mut().push(format!("eject {drive}"));
            Ok(())
        }
    }

    #[test]
    fn eject_unmounts_each_partition_then_powers_the_drive_off() {
        let f = Fake {
            objects: world(),
            ..Fake::default()
        };
        eject_with(&f, "/media/me/STICK", None, "the stick").unwrap();
        assert_eq!(
            *f.calls.borrow(),
            [
                format!("unmount {B1}"),
                format!("unmount {B2}"),
                format!("power_off {D}")
            ]
        );
    }

    #[test]
    fn a_busy_partition_stops_it_before_the_drive_is_touched() {
        let f = Fake {
            objects: world(),
            busy: Some(B2.into()),
            ..Fake::default()
        };
        let e = eject_with(&f, "/media/me/STICK", None, "the stick").unwrap_err();
        assert!(e.contains("still in use"), "{e}");
        assert!(!f.calls.borrow().iter().any(|c| c.starts_with("power_off")));
    }

    #[test]
    fn a_failed_power_off_still_counts_as_unmounted_and_safe() {
        let f = Fake {
            objects: world(),
            power_off_fails: true,
            ..Fake::default()
        };
        assert_eq!(eject_with(&f, "/media/me/STICK", None, "the stick"), Ok(()));
    }

    #[test]
    fn eject_is_the_fallback_only_when_power_off_isnt_available() {
        let mut o = world();
        o.drives[1] = drive(D, true, "usb", false, true);
        let f = Fake {
            objects: o.clone(),
            ..Fake::default()
        };
        eject_with(&f, "/media/me/STICK", None, "the stick").unwrap();
        assert_eq!(f.calls.borrow().last().unwrap(), &format!("eject {D}"));
        // Neither: unmounted is all there is, and that is success.
        o.drives[1] = drive(D, true, "usb", false, false);
        let f = Fake {
            objects: o,
            ..Fake::default()
        };
        eject_with(&f, "/media/me/STICK", None, "the stick").unwrap();
        assert!(f.calls.borrow().iter().all(|c| c.starts_with("unmount")));
    }

    #[test]
    fn without_udisks2_the_message_says_so() {
        let f = Fake {
            down: true,
            ..Fake::default()
        };
        let e = eject_with(&f, "/x", None, "the stick").unwrap_err();
        assert!(e.contains("udisks2") && e.contains("isn't running"), "{e}");
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn udisks_objects_are_read_from_the_bus_reply() {
        let bytes = |s: &str| {
            let mut v: Vec<u8> = s.bytes().collect();
            v.push(0);
            v
        };
        let owned = |v: Value<'_>| OwnedValue::try_from(v).unwrap();
        let mut block_props = Props::new();
        block_props.insert("Device".into(), owned(Value::new(bytes("/dev/sdb1"))));
        block_props.insert(
            "Drive".into(),
            owned(Value::new(zbus::zvariant::ObjectPath::try_from(D).unwrap())),
        );
        let mut fs_props = Props::new();
        fs_props.insert(
            "MountPoints".into(),
            owned(Value::new(vec![bytes("/media/me/STICK")])),
        );
        let mut drive_props = Props::new();
        drive_props.insert("Removable".into(), owned(Value::Bool(true)));
        drive_props.insert("ConnectionBus".into(), owned(Value::new("usb")));
        drive_props.insert("CanPowerOff".into(), owned(Value::Bool(true)));
        let mut all = HashMap::new();
        all.insert(
            OwnedObjectPath::try_from(B1).unwrap(),
            HashMap::from([
                ("org.freedesktop.UDisks2.Block".to_string(), block_props),
                ("org.freedesktop.UDisks2.Filesystem".to_string(), fs_props),
            ]),
        );
        all.insert(
            OwnedObjectPath::try_from(D).unwrap(),
            HashMap::from([("org.freedesktop.UDisks2.Drive".to_string(), drive_props)]),
        );
        let o = parse_objects(all);
        assert_eq!(
            o.blocks,
            [block(B1, "/dev/sdb1", Some(D), &["/media/me/STICK"])]
        );
        assert_eq!(o.drives, [drive(D, true, "usb", true, false)]);
        assert_eq!(plan(&o, "/media/me/STICK", None).unwrap().unmount, [B1]);
    }

    /// Ejects a real drive through the real udisks2. Only the owner runs this, by hand, on a
    /// machine with a spare stick mounted: `PNK_EJECT_MOUNT=/media/me/STICK cargo test -p pnk-core
    /// live_eject -- --ignored`. It unmounts and powers off that stick.
    #[test]
    #[ignore = "ejects a real drive; the owner runs it by hand"]
    fn live_eject_of_a_real_stick() {
        let mount = std::env::var("PNK_EJECT_MOUNT").expect("set PNK_EJECT_MOUNT to a mount point");
        let device = ListedDevice {
            id: mount,
            label: "live".into(),
            kind_hint: Some(crate::signals::DeviceKind::Storage),
            transport: crate::platform::Transport::Volume,
        };
        eject(&device).expect("the eject worked");
    }

    /// Reading what udisks lists changes nothing: it needs the bus and a running udisks2, which
    /// the build machine may not have, so it only checks that asking never panics.
    #[test]
    fn asking_udisks_for_its_objects_does_not_panic() {
        if let Ok(conn) = Connection::system() {
            let _ = Bus(conn).objects();
        }
    }
}
