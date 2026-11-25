//! Plug-in and removal detection on Linux.
//!
//! The kernel announces devices on a netlink socket (the messages udev itself listens to), which
//! needs no library and no rights. A USB device, disk or partition that appears or goes starts a
//! short quiet period, then one wake-up. A slow poll of the mounted volumes runs alongside,
//! because a stick that has just been plugged in is mounted a little later by the desktop. If the
//! socket can't be opened, the poll also covers the phone lists.
//!
//! The watcher only says that something changed; the window lists the devices.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use super::{camera, ios, mtp, volume};
use crate::platform::poll::{Debounce, QUIET, Wake, Watcher, changed, spawn_poll};

/// How often the mounted volumes are looked at.
const EVERY: Duration = Duration::from_secs(3);
/// How often the phone lists are looked at when there is no event socket.
const PHONES_EVERY: Duration = Duration::from_secs(4);

/// One kernel message about a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uevent {
    pub action: String,
    pub subsystem: String,
    pub devpath: String,
}

/// Reads a kernel message: `add@/devices/...`, then `KEY=value` pairs separated by zero bytes.
/// Messages from udev's own daemon (they start with `libudev`) and anything unreadable give
/// `None`.
pub fn parse_uevent(msg: &[u8]) -> Option<Uevent> {
    if msg.starts_with(b"libudev") {
        return None;
    }
    let mut parts = msg.split(|b| *b == 0);
    let head = std::str::from_utf8(parts.next()?).ok()?;
    let (head_action, head_path) = head.split_once('@')?;
    let (mut action, mut subsystem, mut devpath) = (None, None, None);
    for kv in parts {
        let Ok(kv) = std::str::from_utf8(kv) else {
            continue;
        };
        match kv.split_once('=') {
            Some(("ACTION", v)) => action = Some(v.to_string()),
            Some(("SUBSYSTEM", v)) => subsystem = Some(v.to_string()),
            Some(("DEVPATH", v)) => devpath = Some(v.to_string()),
            _ => {}
        }
    }
    Some(Uevent {
        action: action.unwrap_or_else(|| head_action.to_string()),
        subsystem: subsystem?,
        devpath: devpath.unwrap_or_else(|| head_path.to_string()),
    })
}

/// Whether this message can mean a device came or went: a USB device or a disk or partition
/// added or removed.
pub fn is_relevant(e: &Uevent) -> bool {
    matches!(e.action.as_str(), "add" | "remove") && matches!(e.subsystem.as_str(), "usb" | "block")
}

/// The kernel's device-event socket.
struct Socket(OwnedFd);

impl Socket {
    fn open() -> Result<Self, String> {
        // SAFETY: plain system calls; the descriptor is owned by `OwnedFd` as soon as it exists.
        unsafe {
            let fd = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_KOBJECT_UEVENT,
            );
            if fd < 0 {
                return Err(format!(
                    "no device-event socket: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let owned = OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_nl = std::mem::zeroed();
            addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
            addr.nl_groups = 1; // the kernel's own messages
            let rc = libc::bind(
                owned.as_raw_fd(),
                (&raw const addr).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            );
            if rc < 0 {
                return Err(format!(
                    "could not listen for device events: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self(owned))
        }
    }

    /// Waits up to `ms` for a message and reads all that are waiting. `true` if the socket
    /// reported an error (e.g. `ENOBUFS`: events were dropped), so the caller should look at the
    /// devices again anyway.
    fn read(&self, ms: i32, out: &mut Vec<Vec<u8>>) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a live local, and the buffer is as long as the length passed.
        unsafe {
            if libc::poll(&mut pfd, 1, ms) <= 0 {
                return false;
            }
            let mut buf = [0u8; 8192];
            loop {
                let n = libc::recv(
                    self.0.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                );
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    let again = matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    );
                    if !again {
                        warn!("Reading device events failed ({e}); looking at the devices again");
                    }
                    return !again;
                }
                if n == 0 {
                    break;
                }
                out.push(buf[..n as usize].to_vec());
            }
        }
        false
    }
}

fn volume_ids() -> Vec<String> {
    volume::list().into_iter().map(|d| d.id).collect()
}

fn all_ids() -> Vec<String> {
    let mut ids = volume_ids();
    ids.extend(mtp::list().into_iter().map(|d| d.id));
    ids.extend(ios::list().into_iter().map(|d| d.id));
    ids.extend(camera::list().into_iter().map(|d| d.id));
    ids
}

/// Starts watching. `wake` is called, from another thread, when the devices may have changed.
pub fn watch(wake: Wake) -> Result<Watcher, String> {
    let socket = match Socket::open() {
        Ok(s) => s,
        Err(e) => {
            warn!("{e}: looking for devices by polling instead");
            return spawn_poll("device-watch", PHONES_EVERY, all_ids, wake);
        }
    };
    info!("Watching for plugged-in devices through the kernel's device events");
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("device-watch".into())
        .spawn(move || {
            let mut debounce = Debounce::default();
            let mut mounts = volume_ids();
            let mut last_poll = Instant::now();
            let mut msgs = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                msgs.clear();
                let lost = socket.read(250, &mut msgs);
                let now = Instant::now();
                if lost {
                    debounce.event(now);
                }
                for m in &msgs {
                    if let Some(e) = parse_uevent(m)
                        && is_relevant(&e)
                    {
                        debug!("Device event: {} {} {}", e.action, e.subsystem, e.devpath);
                        debounce.event(now);
                    }
                }
                // A stick is mounted a little after it appears: watch the mounts too.
                if now.duration_since(last_poll) >= EVERY {
                    last_poll = now;
                    let ids = volume_ids();
                    if changed(&mounts, &ids) {
                        mounts = ids;
                        debounce.event(now);
                    }
                }
                if debounce.due(Instant::now(), QUIET) {
                    wake();
                }
            }
        })
        .map_err(|e| format!("could not start the device watcher: {e}"))?;
    Ok(Watcher::new(stop, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    #[test]
    fn a_kernel_message_is_read() {
        let m = msg(&[
            "add@/devices/pci0000:00/usb1/1-1",
            "ACTION=add",
            "DEVPATH=/devices/pci0000:00/usb1/1-1",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_device",
            "SEQNUM=4711",
        ]);
        let e = parse_uevent(&m).unwrap();
        assert_eq!(e.action, "add");
        assert_eq!(e.subsystem, "usb");
        assert_eq!(e.devpath, "/devices/pci0000:00/usb1/1-1");
        assert!(is_relevant(&e));
    }

    #[test]
    fn udev_daemon_messages_and_junk_are_ignored() {
        assert_eq!(parse_uevent(b"libudev\0\xfe\xed\xca\xfe"), None);
        assert_eq!(parse_uevent(b""), None);
        assert_eq!(parse_uevent(b"no at sign\0SUBSYSTEM=usb\0"), None);
        assert_eq!(parse_uevent(&msg(&["add@/x", "ACTION=add"])), None);
        // Bytes that are not text are skipped, not fatal.
        let mut m = msg(&["remove@/x", "SUBSYSTEM=block"]);
        m.extend_from_slice(b"\xff\xfe\0");
        assert_eq!(parse_uevent(&m).unwrap().action, "remove");
    }

    /// A camera is a USB device like a phone: its arrival and removal are USB events, so the
    /// list is made again when one is plugged in or taken out.
    #[test]
    fn a_camera_plugged_in_or_out_wakes_the_watcher() {
        for action in ["add", "remove"] {
            let m = msg(&[
                &format!("{action}@/devices/pci0000:00/usb1/1-3"),
                &format!("ACTION={action}"),
                "DEVPATH=/devices/pci0000:00/usb1/1-3",
                "SUBSYSTEM=usb",
                "DEVTYPE=usb_device",
                "PRODUCT=4a9/3234/100",
                "TYPE=0/0/0",
                "BUSNUM=001",
                "DEVNUM=007",
            ]);
            let e = parse_uevent(&m).unwrap();
            assert!(is_relevant(&e), "{action}");
        }
    }

    #[test]
    fn only_devices_coming_or_going_count() {
        let ev = |a: &str, s: &str| Uevent {
            action: a.into(),
            subsystem: s.into(),
            devpath: "/d".into(),
        };
        assert!(is_relevant(&ev("add", "usb")));
        assert!(is_relevant(&ev("remove", "block")));
        assert!(!is_relevant(&ev("change", "block")));
        assert!(!is_relevant(&ev("bind", "usb")));
        assert!(!is_relevant(&ev("add", "net")));
    }

    /// Starting a watcher works (through the event socket, or the poll if that is refused), and
    /// dropping it stops it.
    #[test]
    fn a_watcher_starts_and_stops() {
        let w = watch(Box::new(|| {})).expect("start watching");
        drop(w);
    }
}
