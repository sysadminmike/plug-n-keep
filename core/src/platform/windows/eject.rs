//! Eject for a stick or drive on Windows: the same request Explorer's "Eject" makes.
//!
//! The drive letter gives the disk, the disk gives its Plug and Play device node, and one node on
//! the way up from it is asked to go with `CM_Request_Device_EjectW`. Windows then asks every
//! program and driver that uses the device whether it may; a "no" (a veto) comes back with its
//! kind and, mostly, a name, and is told to the user in plain words.
//!
//! **Which node** ([`choose_node`]) is a pure choice over what each node on the way up is (its
//! instance ID, whether it is marked removable, its driver service and its class), so it is
//! tested for a stick on a port, behind a hub, in a dock, in a composite device and in a card
//! reader. The climb stops at the first USB hub, bus or root: nothing at or above it is ever
//! asked to go, because ejecting a hub would take every other device on it away too. The node
//! is the nearest one Windows marks removable (a stick's USB device node, or its disk node in a
//! card reader), else the nearest USB device; never a composite device's interface (`&MI_`), only
//! the device it belongs to. Explorer's "Safely Remove" could not be checked from here; the
//! needs a check on real hardware.
//!
//! This asks Windows to remove the device node. It is not a write to the device and it is not a
//! change to its content, so it is not one of the calls `clippy.toml` bans. It also never opens
//! the volume: no write handle, no lock and no dismount. The app's promise that a device is
//! read-only covers handles too, and a test reads this file to check that it has none.
//!
//! The Windows calls sit behind [`Pnp`], so the choice of node and the wording of a veto are
//! tested without a device.

use std::ffi::c_void;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVCAP_REMOVABLE, CM_DRP_CAPABILITIES, CM_DRP_CLASSGUID, CM_DRP_SERVICE,
    CM_Get_DevNode_Registry_PropertyW, CM_Get_Parent, CM_Request_Device_EjectW, CONFIGRET,
    CR_REMOVE_VETOED, CR_SUCCESS, PNP_VETO_TYPE,
};

use super::volume::{disk_devinst, disk_facts, instance_id, letter_of_root};
use crate::platform::ListedDevice;

/// One device node on the way up from the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub instance_id: String,
    /// Windows' "removable" capability for the node.
    pub removable: bool,
    /// The node's driver service (`CM_DRP_SERVICE`): `USBHUB3` for a hub, `usbccgp` for a
    /// composite device, `USBSTOR` for a stick. Empty if Windows doesn't say.
    pub service: String,
    /// The node's class GUID (`CM_DRP_CLASSGUID`), as text. Empty if Windows doesn't say.
    pub class_guid: String,
}

impl Node {
    /// A node with only an instance ID and the removable flag (the rest unknown), for tests.
    #[cfg(test)]
    fn bare(id: &str, removable: bool) -> Self {
        Node {
            instance_id: id.into(),
            removable,
            service: String::new(),
            class_guid: String::new(),
        }
    }

    /// A USB hub: its service is the hub driver, or it is a root hub. An external hub, a dock's
    /// hub and a monitor's hub all look the same.
    fn is_hub(&self) -> bool {
        let service = self.service.to_ascii_lowercase();
        matches!(service.as_str(), "usbhub" | "usbhub3" | "usbhub4")
            || self
                .instance_id
                .to_ascii_uppercase()
                .starts_with("USB\\ROOT_HUB")
    }

    /// One interface of a composite USB device (`USB\VID_..&PID_..&MI_00\..`): not the device.
    fn is_interface(&self) -> bool {
        self.instance_id.to_ascii_uppercase().contains("&MI_")
    }
}

/// Whether an instance ID is a bus or root rather than a device on it: eject never goes this high.
fn is_bus_root(id: &str) -> bool {
    let id = id.to_ascii_uppercase();
    [
        "USB\\ROOT_HUB",
        "PCI\\",
        "ACPI\\",
        "ROOT\\",
        "HTREE\\",
        "SWD\\",
        "USB4\\",
    ]
    .iter()
    .any(|p| id.starts_with(p))
}

/// Which node to eject, from the disk's own node and its ancestors (nearest first). The climb
/// stops at the first hub, bus or root, which is never chosen. Below that, the nearest node
/// Windows marks removable: a USB disk is the node itself when it is removable (a stick, a
/// card in a reader) and its USB device above it when it is not (a USB hard drive). If none is
/// marked, the nearest USB device is the fallback. A composite device's interface is skipped for
/// the device it belongs to. `None` if nothing below the first hub qualifies.
pub fn choose_node(chain: &[Node]) -> Option<usize> {
    let below_the_hub = chain
        .iter()
        .position(|n| is_bus_root(&n.instance_id) || n.is_hub())
        .unwrap_or(chain.len());
    let candidates = &chain[..below_the_hub];
    let own = |n: &&Node| !n.is_interface();
    candidates
        .iter()
        .position(|n| n.removable && own(&n))
        .or_else(|| {
            candidates
                .iter()
                .position(|n| own(&n) && n.instance_id.to_ascii_uppercase().starts_with("USB\\"))
        })
}

/// `CR_NO_SUCH_DEVNODE` and `CR_ACCESS_DENIED` as the `CONFIGRET` numbers they are.
const CR_NO_SUCH_DEVNODE_CODE: u32 = 0x0D;
const CR_ACCESS_DENIED_CODE: u32 = 0x33;

/// What Windows answered to the eject request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Done,
    /// Something vetoed it: the kind (`PNP_VETO_TYPE`) and the name Windows gave, if any.
    Vetoed {
        kind: i32,
        name: String,
    },
    /// Any other failure: the `CONFIGRET` code.
    Failed(u32),
}

/// The Windows calls, as the eject needs them.
pub trait Pnp {
    /// The device nodes from the disk behind `letter` up to the root, nearest first, each with
    /// the handle to ask for it. Fails with a sentence for the user.
    fn chain(&self, letter: char) -> Result<Vec<(Node, u32)>, String>;
    /// Asks Windows to eject one node.
    fn eject(&self, devinst: u32) -> Answer;
}

/// A veto in plain words. `what` is "the drive E:\".
pub fn veto_message(kind: i32, name: &str, what: &str) -> String {
    let name = name.trim();
    let named = |by: &str| {
        if name.is_empty() {
            format!(
                "Windows says {what} is still in use, but not by what. Close any program or \
                 window that uses it (File Explorer included) and try again."
            )
        } else {
            format!("Windows says {what} is still in use by {by} {name}. Close it and try again.")
        }
    };
    match kind {
        3 => named("the program"),
        4 => named("the Windows service"),
        2 | 5 => {
            if name.is_empty() {
                named("")
            } else {
                format!(
                    "Windows says {what} is still in use ({name}). Close any program or window \
                     that uses it (File Explorer included) and try again."
                )
            }
        }
        1 | 6 => named("the device"),
        7 | 11 => named("the driver"),
        9 => format!("Windows says there is not enough power to eject {what}."),
        10 => format!("Windows says {what} can't be stopped: the system needs it."),
        _ => {
            if name.is_empty() {
                format!("Windows refused to eject {what} (reason {kind}).")
            } else {
                format!("Windows refused to eject {what}: {name} (reason {kind}).")
            }
        }
    }
}

/// Ejects the drive `letter` (shown to the user as `what`) through `pnp`.
pub fn eject_with(pnp: &dyn Pnp, letter: char, what: &str) -> Result<(), String> {
    let chain = pnp.chain(letter)?;
    let nodes: Vec<Node> = chain.iter().map(|(n, _)| n.clone()).collect();
    let Some(i) = choose_node(&nodes) else {
        return Err(format!(
            "Windows doesn't offer to eject {what}: it is not a removable device. Use the \
             \"Safely Remove Hardware\" icon, or just unplug it once nothing is using it."
        ));
    };
    let (node, devinst) = &chain[i];
    log::info!(
        "Asking Windows to eject {what} (device {})",
        node.instance_id
    );
    match pnp.eject(*devinst) {
        Answer::Done => Ok(()),
        // `CR_NO_SUCH_DEVNODE`: the device is already gone, which is what was wanted.
        Answer::Failed(CR_NO_SUCH_DEVNODE_CODE) => Ok(()),
        // `CR_ACCESS_DENIED`.
        Answer::Failed(CR_ACCESS_DENIED_CODE) => Err(format!(
            "Windows won't let this account eject {what}. Try again from an account that may \
             remove devices."
        )),
        Answer::Vetoed { kind, name } => {
            log::warn!("Windows vetoed ejecting {what}: type {kind}, \"{name}\"");
            Err(veto_message(kind, &name, what))
        }
        Answer::Failed(code) => {
            log::warn!("Ejecting {what} failed: configuration manager code {code}");
            Err(format!(
                "Windows could not eject {what} (error {code}). Close anything that uses it and \
                 try again."
            ))
        }
    }
}

/// Windows' removable flag for a device node.
fn removable(devinst: u32) -> bool {
    let mut caps = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: the buffer is a live u32 and `len` says so.
    let rc = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            devinst,
            CM_DRP_CAPABILITIES,
            None,
            Some((&raw mut caps).cast::<c_void>()),
            &mut len,
            0,
        )
    };
    rc == CR_SUCCESS && caps & CM_DEVCAP_REMOVABLE.0 != 0
}

/// A text property of a device node (`CM_DRP_SERVICE`, `CM_DRP_CLASSGUID`), or empty.
fn text_property(devinst: u32, property: u32) -> String {
    let mut buf = [0u16; 260];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: the buffer is a live array and `len` is its size in bytes.
    let rc = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            devinst,
            property,
            None,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            &mut len,
            0,
        )
    };
    if rc != CR_SUCCESS {
        return String::new();
    }
    let chars = (len as usize / 2).min(buf.len());
    let end = buf[..chars].iter().position(|&c| c == 0).unwrap_or(chars);
    String::from_utf16_lossy(&buf[..end])
}

struct Windows;

impl Pnp for Windows {
    fn chain(&self, letter: char) -> Result<Vec<(Node, u32)>, String> {
        let disk = disk_facts(letter)
            .disk_number
            .and_then(disk_devinst)
            .ok_or_else(|| {
                format!("Windows doesn't say which device {letter}: is, so it can't be ejected.")
            })?;
        let mut out = Vec::new();
        let mut inst = disk;
        for _ in 0..8 {
            out.push((
                Node {
                    instance_id: instance_id(inst).unwrap_or_default(),
                    removable: removable(inst),
                    service: text_property(inst, CM_DRP_SERVICE),
                    class_guid: text_property(inst, CM_DRP_CLASSGUID),
                },
                inst,
            ));
            let mut parent = 0u32;
            // SAFETY: plain query.
            if unsafe { CM_Get_Parent(&mut parent, inst, 0) } != CR_SUCCESS {
                break;
            }
            inst = parent;
        }
        Ok(out)
    }

    fn eject(&self, devinst: u32) -> Answer {
        let mut veto = PNP_VETO_TYPE(0);
        let mut name = [0u16; 260];
        // SAFETY: the veto type and the name buffer are live locals of the stated sizes.
        let rc: CONFIGRET =
            unsafe { CM_Request_Device_EjectW(devinst, Some(&mut veto), Some(&mut name), 0) };
        if rc == CR_SUCCESS {
            Answer::Done
        } else if rc == CR_REMOVE_VETOED {
            let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            Answer::Vetoed {
                kind: veto.0,
                name: String::from_utf16_lossy(&name[..end]),
            }
        } else {
            Answer::Failed(rc.0)
        }
    }
}

/// Ejects the stick or drive `device` (its ID is the drive root, `E:\`).
pub fn eject(device: &ListedDevice) -> Result<(), String> {
    let letter = letter_of_root(&device.id)
        .ok_or_else(|| format!("{} is not a drive that can be ejected", device.id))?;
    eject_with(&Windows, letter, &format!("the drive {}", device.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn node(id: &str, removable: bool) -> Node {
        Node::bare(id, removable)
    }

    /// A node with the driver service Windows gives it.
    fn served(id: &str, removable: bool, service: &str) -> Node {
        Node {
            service: service.into(),
            ..Node::bare(id, removable)
        }
    }

    #[test]
    fn a_removable_stick_is_ejected_at_its_own_node() {
        let chain = [
            node("USBSTOR\\DISK&VEN_X&PROD_STICK\\123&0", true),
            node("USB\\VID_0781&PID_5567\\ABC", true),
            node("USB\\ROOT_HUB30\\4&1", false),
        ];
        assert_eq!(choose_node(&chain), Some(0));
    }

    #[test]
    fn a_usb_hard_drive_is_ejected_at_the_usb_device_above_its_disk() {
        let chain = [
            node("SCSI\\DISK&VEN_WD\\1", false),
            node("USB\\VID_1058&PID_25A2\\XYZ", true),
            node("USB\\VID_05E3&PID_0610\\HUB", true),
            node("USB\\ROOT_HUB30\\4&1", false),
        ];
        assert_eq!(choose_node(&chain), Some(1));
    }

    #[test]
    fn nothing_marked_removable_falls_back_to_the_nearest_usb_device() {
        let chain = [
            node("USBSTOR\\DISK\\1", false),
            node("USB\\VID_0781&PID_5567\\ABC", false),
            node("USB\\ROOT_HUB30\\4&1", false),
        ];
        assert_eq!(choose_node(&chain), Some(1));
    }

    #[test]
    fn a_bus_or_root_is_never_chosen() {
        // A card reader on the PCI bus: nothing below the bus is removable or USB.
        let chain = [
            node("SD\\DISK\\1", false),
            node("PCI\\VEN_10EC&DEV_5229\\3", true),
            node("ACPI\\PNP0A08\\0", true),
        ];
        assert_eq!(choose_node(&chain), None);
        // A removable node above a root is out of reach too.
        let chain = [node("USB\\ROOT_HUB30\\4&1", true), node("PCI\\X", true)];
        assert_eq!(choose_node(&chain), None);
        assert_eq!(choose_node(&[]), None);
    }

    /// The bug: a stick behind a hub, where neither the disk nor the stick's own USB node is
    /// marked removable but the hub is. The old choice took the hub and would have taken every
    /// other device on it away. The hub is never chosen; the stick's own node is.
    #[test]
    fn a_stick_behind_an_external_hub_never_gets_the_hub() {
        let chain = [
            served("USBSTOR\\DISK&VEN_X\\1", false, "disk"),
            served("USB\\VID_0781&PID_5567\\ABC", false, "USBSTOR"),
            served("USB\\VID_05E3&PID_0610\\HUBSERIAL", true, "USBHUB3"),
            served("USB\\ROOT_HUB30\\4&1", false, "USBHUB3"),
        ];
        assert_eq!(choose_node(&chain), Some(1));
        // The hub's service decides, in any case; lower case too.
        let mut c = chain.clone();
        c[2].service = "usbhub".into();
        assert_eq!(choose_node(&c), Some(1));
        // Even a stick whose own nodes are marked removable stops below the hub.
        let mut c = chain.clone();
        c[1].removable = true;
        assert_eq!(choose_node(&c), Some(1));
        // And with the stick's nodes unusable, the hub is still not the answer.
        let c = [
            served("SD\\DISK\\1", false, "disk"),
            served("USB\\VID_05E3&PID_0610\\HUBSERIAL", true, "USBHUB3"),
        ];
        assert_eq!(choose_node(&c), None);
    }

    #[test]
    fn a_stick_in_a_docks_hub_is_ejected_at_the_stick() {
        let chain = [
            served("USBSTOR\\DISK\\1", true, "disk"),
            served("USB\\VID_0781&PID_5567\\ABC", true, "USBSTOR"),
            served("USB\\VID_17EF&PID_A387\\DOCKHUB", true, "USBHUB3"),
            served("USB\\VID_17EF&PID_A388\\DOCK", true, "usbccgp"),
            served("USB\\ROOT_HUB30\\4&1", false, "USBHUB3"),
        ];
        assert_eq!(choose_node(&chain), Some(0));
    }

    #[test]
    fn a_composite_device_is_ejected_as_the_device_not_as_one_interface() {
        let chain = [
            served("SCSI\\DISK\\1", false, "disk"),
            served("USB\\VID_1058&PID_25A2&MI_00\\7&2", true, "USBSTOR"),
            served("USB\\VID_1058&PID_25A2\\XYZ", true, "usbccgp"),
            served("USB\\ROOT_HUB30\\4&1", false, "USBHUB3"),
        ];
        assert_eq!(choose_node(&chain), Some(2));
        // Nothing marked: the nearest USB node that is a device, not an interface.
        let mut c = chain.clone();
        c[1].removable = false;
        c[2].removable = false;
        assert_eq!(choose_node(&c), Some(2));
    }

    #[test]
    fn a_card_in_a_reader_is_ejected_at_the_card_not_the_reader() {
        let chain = [
            served("USBSTOR\\DISK&VEN_GENERIC&PROD_SD\\LUN1", true, "disk"),
            served("USB\\VID_058F&PID_6366\\READER", true, "USBSTOR"),
            served("USB\\ROOT_HUB30\\4&1", false, "USBHUB3"),
        ];
        assert_eq!(choose_node(&chain), Some(0));
        // A reader whose disk isn't marked removable: the reader itself.
        let mut c = chain.clone();
        c[0].removable = false;
        assert_eq!(choose_node(&c), Some(1));
    }

    #[test]
    fn the_newer_roots_are_never_chosen() {
        for root in ["SWD\\USBDEVICE\\1", "USB4\\ROUTER\\1"] {
            let chain = [node("SD\\DISK\\1", false), node(root, true)];
            assert_eq!(choose_node(&chain), None, "{root}");
        }
    }

    #[test]
    fn a_veto_is_told_in_plain_words_with_the_name_when_there_is_one() {
        let what = "the drive E:\\";
        let m = veto_message(3, "backup.exe", what);
        assert!(m.contains("still in use by the program backup.exe"), "{m}");
        let m = veto_message(4, "SearchIndexer", what);
        assert!(m.contains("Windows service SearchIndexer"), "{m}");
        let m = veto_message(5, "\\Device\\HarddiskVolume9", what);
        assert!(
            m.contains("still in use (\\Device\\HarddiskVolume9)"),
            "{m}"
        );
        let m = veto_message(5, "", what);
        assert!(
            m.contains("still in use") && m.contains("File Explorer"),
            "{m}"
        );
        assert!(veto_message(9, "", what).contains("power"));
        assert!(veto_message(10, "", what).contains("can't be stopped"));
        assert!(veto_message(8, "x", what).contains("reason 8"));
    }

    /// A stand-in for Windows that records what it was asked.
    struct Fake {
        chain: Result<Vec<(Node, u32)>, String>,
        answer: Answer,
        asked: RefCell<Vec<u32>>,
    }

    impl Pnp for Fake {
        fn chain(&self, _letter: char) -> Result<Vec<(Node, u32)>, String> {
            self.chain.clone()
        }
        fn eject(&self, devinst: u32) -> Answer {
            self.asked.borrow_mut().push(devinst);
            self.answer.clone()
        }
    }

    fn fake(answer: Answer) -> Fake {
        Fake {
            chain: Ok(vec![
                (node("USBSTOR\\DISK\\1", true), 11),
                (node("USB\\VID_0781&PID_5567\\ABC", true), 22),
            ]),
            answer,
            asked: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn eject_asks_windows_about_the_chosen_node_only() {
        let f = fake(Answer::Done);
        assert_eq!(eject_with(&f, 'E', "the drive E:\\"), Ok(()));
        assert_eq!(*f.asked.borrow(), [11]);
    }

    #[test]
    fn a_veto_is_an_error_with_its_reason_and_a_gone_device_is_success() {
        let f = fake(Answer::Vetoed {
            kind: 3,
            name: "winword.exe".into(),
        });
        let e = eject_with(&f, 'E', "the drive E:\\").unwrap_err();
        assert!(e.contains("winword.exe"), "{e}");
        // No such device node: it is already gone, which is what was wanted.
        let f = fake(Answer::Failed(0x0D));
        assert_eq!(eject_with(&f, 'E', "the drive E:\\"), Ok(()));
        // Access denied has its own plain words.
        let f = fake(Answer::Failed(0x33));
        let e = eject_with(&f, 'E', "the drive E:\\").unwrap_err();
        assert!(e.contains("this account"), "{e}");
        let f = fake(Answer::Failed(5));
        let e = eject_with(&f, 'E', "the drive E:\\").unwrap_err();
        assert!(e.contains("error 5"), "{e}");
    }

    #[test]
    fn nothing_is_asked_when_no_node_can_be_ejected_or_the_disk_is_unknown() {
        let mut f = fake(Answer::Done);
        f.chain = Ok(vec![(node("PCI\\X", true), 1)]);
        let e = eject_with(&f, 'E', "the drive E:\\").unwrap_err();
        assert!(e.contains("not a removable device"), "{e}");
        assert!(f.asked.borrow().is_empty());
        f.chain = Err("no disk".into());
        assert_eq!(eject_with(&f, 'E', "x").unwrap_err(), "no disk");
        assert!(f.asked.borrow().is_empty());
    }

    /// The read-only promise covers handles: ejecting opens nothing and sends no control code.
    #[test]
    fn ejecting_never_opens_locks_or_dismounts_a_volume() {
        let src = include_str!("eject.rs").replace("\r\n", "\n");
        let prod = &src[..src
            .find(
                "#[cfg(test)]
mod tests",
            )
            .unwrap()];
        for banned in [
            "CreateFile",
            "DeviceIoControl",
            "FSCTL_",
            "IOCTL_",
            "GENERIC_WRITE",
            "GENERIC_READ",
            "FlushFileBuffers",
            "SetVolume",
            "DeleteVolume",
        ] {
            assert!(!prod.contains(banned), "{banned} must not appear");
        }
        // The one request that changes anything is the eject itself, and it is made once.
        assert_eq!(prod.matches("CM_Request_Device_EjectW(").count(), 1);
    }
}
