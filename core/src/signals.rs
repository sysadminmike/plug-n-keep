//! What can tell one device from another: the signals of DESIGN.md section 5.1, how strong each
//! one is, and the pure helpers that read them out of strings Windows gives us.
//!
//! Nothing here talks to a device. The functions take sample strings, so they are tested with
//! made-up values.
#![forbid(unsafe_code)]

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// What kind of device it is. This sets defaults only (what to back up, how to compare, the
/// archive layout); nothing that keeps data safe depends on it, because a device can claim to be
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// A phone or camera that speaks PTP (an iPhone, many cameras).
    Ptp,
    /// A phone or player that speaks MTP (an Android phone).
    Mtp,
    /// A USB stick, SD card or drive, read through the file system.
    Storage,
    /// A still camera that speaks PTP, read through libgphoto2 (Linux).
    Camera,
}

impl DeviceKind {
    pub fn label(self) -> &'static str {
        match self {
            DeviceKind::Ptp => "phone or camera (PTP)",
            DeviceKind::Mtp => "phone or player (MTP)",
            DeviceKind::Storage => "storage",
            DeviceKind::Camera => "camera",
        }
    }

    /// A best guess for a device whose kind wasn't stored, such as one rebuilt from a marker
    /// file: only a drive has a file-system signal.
    pub fn guess_from(signals: &Signals) -> DeviceKind {
        let is_storage = signals.volume_serial.is_some()
            || signals.gpt_partition_id.is_some()
            || signals.volume_guid.is_some()
            || signals.label.is_some();
        if is_storage {
            DeviceKind::Storage
        } else if signals
            .maker
            .as_deref()
            .is_some_and(|m| m.to_lowercase().contains("apple"))
        {
            DeviceKind::Ptp
        } else {
            DeviceKind::Mtp
        }
    }
}

impl Signals {
    /// Whether these signals describe an Apple device: Apple's USB vendor ID, or "Apple" or
    /// "iPhone" in the maker or model. Only a hint, for defaults: any device can claim it.
    pub fn looks_like_apple(&self) -> bool {
        let said = |v: &Option<String>, what: &str| {
            v.as_deref()
                .is_some_and(|t| t.to_lowercase().contains(what))
        };
        said(&self.maker, "apple")
            || said(&self.model, "iphone")
            || said(&self.model, "apple")
            || said(&self.friendly_name, "iphone")
            || self
                .usb_vendor
                .as_deref()
                .is_some_and(|v| v.eq_ignore_ascii_case("05ac"))
    }
}

/// How a device was read. A phone looks different through each (an iPhone shows month folders
/// through Apple's Windows driver and its own `DCIM` folders through AFC), so a backup folder is
/// only synced through the view it was made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum View {
    /// Windows Portable Devices.
    #[serde(rename = "windows-wpd")]
    WindowsWpd,
    #[serde(rename = "mtp")]
    Mtp,
    /// An iPhone through usbmuxd and AFC.
    #[serde(rename = "afc")]
    Afc,
    /// A mounted file system.
    #[serde(rename = "files")]
    Files,
    /// A still camera through libgphoto2: its folders are the camera's own.
    #[serde(rename = "camera")]
    Camera,
}

impl View {
    /// The name stored in the marker.
    pub fn name(self) -> &'static str {
        match self {
            View::WindowsWpd => "windows-wpd",
            View::Mtp => "mtp",
            View::Afc => "afc",
            View::Files => "files",
            View::Camera => "camera",
        }
    }
}

/// Every signal the app can collect. Each field is optional: only the ones a device offers are
/// filled in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Signals {
    /// The `device_id` in the ID file at the top of a stick.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_file_id: Option<String>,
    /// The serial number WPD reports after a read-only open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_serial: Option<String>,
    /// The serial part of the USB instance ID, if there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb_serial: Option<String>,
    /// USB vendor ID, four hex digits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb_vendor: Option<String>,
    /// USB product ID, four hex digits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb_product: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpt_partition_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub friendly_name: Option<String>,
    /// The volume label of a drive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Total size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
    /// The names of the device's storages ("Internal storage", "SD card").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub storage_names: Vec<String>,
}

/// One kind of signal, for naming the one that decided a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    IdFile,
    DeviceSerial,
    UsbSerial,
    GptPartitionId,
    VolumeGuid,
    VolumeSerial,
    /// The vendor and product ID together: they name a model, not one unit.
    UsbVendorProduct,
    Maker,
    Model,
    FriendlyName,
    Label,
    Capacity,
    StorageNames,
}

impl SignalKind {
    /// For messages: "Recognised by its serial number".
    pub fn describe(self) -> &'static str {
        match self {
            SignalKind::IdFile => "its ID file",
            SignalKind::DeviceSerial => "its serial number",
            SignalKind::UsbSerial => "its USB serial number",
            SignalKind::GptPartitionId => "its partition ID",
            SignalKind::VolumeGuid => "its volume ID on this PC",
            SignalKind::VolumeSerial => "its format ID (volume serial)",
            SignalKind::UsbVendorProduct => "its USB model",
            SignalKind::Maker => "its maker",
            SignalKind::Model => "its model",
            SignalKind::FriendlyName => "its name",
            SignalKind::Label => "its label",
            SignalKind::Capacity => "its size",
            SignalKind::StorageNames => "its storage names",
        }
    }
}

/// How much a signal's equality proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    Weak,
    Strong,
    /// The ID file: the app wrote it for exactly this purpose.
    Strongest,
}

impl Signals {
    /// Every signal that is present, as `(kind, normalised value)`. Values are trimmed and
    /// lower-cased, so two spellings of one value compare equal. Empty values are left out.
    pub fn values(&self) -> Vec<(SignalKind, String)> {
        let mut out = Vec::new();
        let mut put = |kind: SignalKind, v: Option<&str>| {
            if let Some(v) = v {
                let n = normalise(v);
                if !n.is_empty() {
                    out.push((kind, n));
                }
            }
        };
        put(SignalKind::IdFile, self.id_file_id.as_deref());
        put(SignalKind::DeviceSerial, self.device_serial.as_deref());
        put(SignalKind::UsbSerial, self.usb_serial.as_deref());
        put(SignalKind::GptPartitionId, self.gpt_partition_id.as_deref());
        put(SignalKind::VolumeGuid, self.volume_guid.as_deref());
        put(SignalKind::VolumeSerial, self.volume_serial.as_deref());
        if let (Some(v), Some(p)) = (&self.usb_vendor, &self.usb_product) {
            let both = format!("{}:{}", normalise(v), normalise(p));
            put(SignalKind::UsbVendorProduct, Some(&both));
        }
        put(SignalKind::Maker, self.maker.as_deref());
        put(SignalKind::Model, self.model.as_deref());
        put(SignalKind::FriendlyName, self.friendly_name.as_deref());
        put(SignalKind::Label, self.label.as_deref());
        if let Some(c) = self.capacity {
            put(SignalKind::Capacity, Some(&c.to_string()));
        }
        if !self.storage_names.is_empty() {
            let mut names: Vec<String> = self
                .storage_names
                .iter()
                .map(|n| normalise(n))
                .filter(|n| !n.is_empty())
                .collect();
            names.sort();
            put(SignalKind::StorageNames, Some(&names.join("|")));
        }
        out
    }
}

fn normalise(s: &str) -> String {
    s.trim().to_lowercase()
}

/// The parts of a USB instance ID.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbIds {
    pub vendor: Option<String>,
    pub product: Option<String>,
    /// Only a serial the device itself reports: an instance part that Windows made up (it
    /// contains `&`) is not a serial and is left out. A placeholder is still returned here;
    /// [`is_real_serial`] decides about those.
    pub serial: Option<String>,
}

/// Windows only in format: the Windows platform layer calls this; other systems have their
/// own readers. Read `USB\VID_05AC&PID_12A8\00008030000A1234`. Also accepts the form WPD uses in a device
/// path, `\\?\usb#vid_05ac&pid_12a8#00008030000a1234#{guid}`. Anything else gives no parts.
pub fn parse_windows_usb_instance_id(id: &str) -> UsbIds {
    let id = id.trim().trim_start_matches(r"\\?\");
    let mut parts = id.split(['\\', '#']);
    let Some(bus) = parts.next() else {
        return UsbIds::default();
    };
    if !bus.eq_ignore_ascii_case("usb") {
        return UsbIds::default();
    }
    let Some(ids) = parts.next() else {
        return UsbIds::default();
    };
    let (mut vendor, mut product) = (None, None);
    for token in ids.split('&') {
        let upper = token.to_ascii_uppercase();
        if let Some(v) = upper.strip_prefix("VID_") {
            vendor = hex4(v);
        } else if let Some(p) = upper.strip_prefix("PID_") {
            product = hex4(p);
        }
    }
    let serial = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('{') && !s.contains('&'))
        .map(str::to_string);
    UsbIds {
        vendor,
        product,
        serial,
    }
}

fn hex4(s: &str) -> Option<String> {
    (s.len() == 4 && s.chars().all(|c| c.is_ascii_hexdigit())).then(|| s.to_lowercase())
}

/// A serial that was never meant to identify one unit: too short, one character repeated (all
/// zeros included), a run of consecutive characters, or a well-known filler value.
pub fn is_placeholder_serial(serial: &str) -> bool {
    let s = serial.trim().to_lowercase();
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 6 {
        return true;
    }
    if chars.iter().all(|&c| c == chars[0]) {
        return true;
    }
    if chars.windows(2).all(|w| w[1] as u32 == w[0] as u32 + 1) {
        return true;
    }
    const FILLERS: [&str; 6] = [
        "0123456789abcdef",
        "123456789abcdef0",
        "0123456789abcdefghij",
        "1234567890",
        "1234567890abcdef",
        "00000000000000000001",
    ];
    FILLERS.contains(&s.as_str())
}

/// Whether `serial` can be trusted to name one unit: not a placeholder and not one Windows made
/// up (those contain `&`).
pub fn is_real_serial(serial: &str) -> bool {
    !serial.contains('&') && !is_placeholder_serial(serial)
}

/// How much equality of this signal proves, or `None` if it proves nothing and is ignored (a
/// serial that isn't real, an empty value).
///
/// - `device` is the kind of device being identified: a volume serial is strong only for storage.
/// - `shared_usb_serials` are the (normalised) USB serials already seen on two known devices.
///   Such a serial counts as weak: some cheap sticks share one across a batch.
pub fn strength(
    kind: SignalKind,
    value: &str,
    device: DeviceKind,
    shared_usb_serials: &HashSet<String>,
) -> Option<Strength> {
    let value = normalise(value);
    if value.is_empty() {
        return None;
    }
    Some(match kind {
        SignalKind::IdFile => Strength::Strongest,
        SignalKind::DeviceSerial => {
            if is_real_serial(&value) {
                Strength::Strong
            } else {
                return None;
            }
        }
        SignalKind::UsbSerial => {
            if !is_real_serial(&value) {
                return None;
            }
            if shared_usb_serials.contains(&value) {
                Strength::Weak
            } else {
                Strength::Strong
            }
        }
        SignalKind::GptPartitionId | SignalKind::VolumeGuid => Strength::Strong,
        SignalKind::VolumeSerial => {
            // A FAT volume serial is written "ABCD-1234": zeros mean none was set.
            let digits: String = value.chars().filter(|c| *c != '-').collect();
            if digits.is_empty() || digits.chars().all(|c| c == '0') {
                return None;
            }
            if device == DeviceKind::Storage {
                Strength::Strong
            } else {
                Strength::Weak
            }
        }
        SignalKind::UsbVendorProduct
        | SignalKind::Maker
        | SignalKind::Model
        | SignalKind::FriendlyName
        | SignalKind::Label
        | SignalKind::Capacity
        | SignalKind::StorageNames => Strength::Weak,
    })
}

/// Whether a signal of this kind can count as a strong *clash* when it differs. The volume ID is
/// only meaningful on the PC that made it, so a different one proves nothing.
pub fn can_clash(kind: SignalKind) -> bool {
    kind != SignalKind::VolumeGuid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn a_usb_instance_id_gives_vendor_product_and_serial() {
        let ids = parse_windows_usb_instance_id(r"USB\VID_05AC&PID_12A8\00008030000A1234");
        assert_eq!(ids.vendor.as_deref(), Some("05ac"));
        assert_eq!(ids.product.as_deref(), Some("12a8"));
        assert_eq!(ids.serial.as_deref(), Some("00008030000A1234"));
    }

    #[test]
    fn a_windows_made_instance_part_is_not_a_serial() {
        let ids = parse_windows_usb_instance_id(r"USB\VID_0781&PID_5581\6&2a1b3c4d&0&2");
        assert_eq!(ids.vendor.as_deref(), Some("0781"));
        assert_eq!(ids.product.as_deref(), Some("5581"));
        assert_eq!(ids.serial, None);
        assert!(!is_real_serial("6&2a1b3c4d&0&2"));
    }

    #[test]
    fn the_wpd_device_path_form_is_read_too() {
        let ids = parse_windows_usb_instance_id(
            r"\\?\usb#vid_05ac&pid_12a8#00008030000a1234#{6ac27878-a6fa-4155-ba85-f98f491d4f33}",
        );
        assert_eq!(ids.vendor.as_deref(), Some("05ac"));
        assert_eq!(ids.product.as_deref(), Some("12a8"));
        assert_eq!(ids.serial.as_deref(), Some("00008030000a1234"));
    }

    #[test]
    fn a_composite_interface_id_still_gives_vendor_and_product() {
        let ids = parse_windows_usb_instance_id(r"USB\VID_18D1&PID_4EE1&MI_00\7&1&0&0000");
        assert_eq!(ids.vendor.as_deref(), Some("18d1"));
        assert_eq!(ids.product.as_deref(), Some("4ee1"));
        assert_eq!(ids.serial, None);
    }

    #[test]
    fn ids_that_are_not_usb_or_are_malformed_give_nothing() {
        for s in [
            "",
            r"SWD\WPDBUSENUM\_??_USBSTOR#DISK",
            r"USB",
            r"USB\ROOT_HUB30\4&1",
            "fake:C:\\x",
        ] {
            assert_eq!(parse_windows_usb_instance_id(s), UsbIds::default(), "{s}");
        }
        // A bad product ID is dropped, the good vendor ID kept.
        let ids = parse_windows_usb_instance_id(r"USB\VID_05AC&PID_12\ABC123456");
        assert_eq!(ids.vendor.as_deref(), Some("05ac"));
        assert_eq!(ids.product, None);
    }

    #[test]
    fn placeholder_serials_are_recognised() {
        for s in [
            "",
            "12345",
            "000000000000",
            "AAAAAAAA",
            "0123456789ABCDEF",
            "0123456789abcdef",
            "123456",
            "  0000  ",
            "1234567890",
        ] {
            assert!(is_placeholder_serial(s), "{s:?}");
        }
        for s in ["00008030000A1234", "AA040000000001234", "4C530001230815"] {
            assert!(!is_placeholder_serial(s), "{s:?}");
            assert!(is_real_serial(s), "{s:?}");
        }
    }

    #[test]
    fn a_real_usb_serial_is_strong_and_a_fake_one_is_ignored() {
        let k = DeviceKind::Storage;
        assert_eq!(
            strength(SignalKind::UsbSerial, "AA040000000001234", k, &none()),
            Some(Strength::Strong)
        );
        assert_eq!(
            strength(SignalKind::UsbSerial, "0123456789ABCDEF", k, &none()),
            None
        );
        assert_eq!(
            strength(SignalKind::UsbSerial, "6&2a1b&0&2", k, &none()),
            None
        );
        assert_eq!(
            strength(SignalKind::DeviceSerial, "00000000", k, &none()),
            None
        );
    }

    #[test]
    fn a_shared_usb_serial_is_only_weak() {
        let shared: HashSet<String> = ["aa040000000001234".to_string()].into();
        assert_eq!(
            strength(
                SignalKind::UsbSerial,
                "AA040000000001234",
                DeviceKind::Storage,
                &shared
            ),
            Some(Strength::Weak)
        );
    }

    #[test]
    fn strengths_of_the_other_signals() {
        let (k, s) = (DeviceKind::Storage, none());
        let st = |kind, v| strength(kind, v, k, &s);
        assert_eq!(st(SignalKind::IdFile, "5f1c"), Some(Strength::Strongest));
        assert_eq!(
            st(SignalKind::DeviceSerial, "ABC12345"),
            Some(Strength::Strong)
        );
        assert_eq!(st(SignalKind::GptPartitionId, "g"), Some(Strength::Strong));
        assert_eq!(st(SignalKind::VolumeGuid, "g"), Some(Strength::Strong));
        assert_eq!(
            st(SignalKind::VolumeSerial, "1A2B-3C4D"),
            Some(Strength::Strong)
        );
        assert_eq!(st(SignalKind::VolumeSerial, "0000-0000"), None);
        for kind in [
            SignalKind::UsbVendorProduct,
            SignalKind::Maker,
            SignalKind::Model,
            SignalKind::FriendlyName,
            SignalKind::Label,
            SignalKind::Capacity,
            SignalKind::StorageNames,
        ] {
            assert_eq!(st(kind, "x"), Some(Strength::Weak), "{kind:?}");
        }
        assert_eq!(st(SignalKind::Maker, "  "), None);
        // A volume serial on a phone is not a storage signal.
        assert_eq!(
            strength(SignalKind::VolumeSerial, "1A2B-3C4D", DeviceKind::Mtp, &s),
            Some(Strength::Weak)
        );
        assert!(Strength::Strongest > Strength::Strong && Strength::Strong > Strength::Weak);
    }

    #[test]
    fn values_are_normalised_and_skip_empties() {
        let s = Signals {
            device_serial: Some("  ABC123  ".into()),
            usb_vendor: Some("05AC".into()),
            usb_product: Some("12A8".into()),
            maker: Some("".into()),
            capacity: Some(64),
            storage_names: vec!["SD card".into(), "Internal".into()],
            ..Signals::default()
        };
        let v = s.values();
        assert!(v.contains(&(SignalKind::DeviceSerial, "abc123".into())));
        assert!(v.contains(&(SignalKind::UsbVendorProduct, "05ac:12a8".into())));
        assert!(v.contains(&(SignalKind::Capacity, "64".into())));
        assert!(v.contains(&(SignalKind::StorageNames, "internal|sd card".into())));
        assert!(!v.iter().any(|(k, _)| *k == SignalKind::Maker));
    }

    #[test]
    fn views_are_stored_under_their_documented_names() {
        for (v, name) in [
            (View::WindowsWpd, "windows-wpd"),
            (View::Mtp, "mtp"),
            (View::Afc, "afc"),
            (View::Files, "files"),
            (View::Camera, "camera"),
        ] {
            assert_eq!(v.name(), name);
            assert_eq!(serde_json::to_string(&v).unwrap(), format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<View>(&format!("\"{name}\"")).unwrap(),
                v
            );
        }
    }

    #[test]
    fn the_kind_is_guessed_from_the_signals() {
        let stick = Signals {
            volume_serial: Some("1A2B-3C4D".into()),
            ..Signals::default()
        };
        assert_eq!(DeviceKind::guess_from(&stick), DeviceKind::Storage);
        let iphone = Signals {
            maker: Some("Apple Inc.".into()),
            ..Signals::default()
        };
        assert_eq!(DeviceKind::guess_from(&iphone), DeviceKind::Ptp);
        assert_eq!(DeviceKind::guess_from(&Signals::default()), DeviceKind::Mtp);
    }

    #[test]
    fn signals_round_trip_through_json_and_old_files_load() {
        let s = Signals {
            device_serial: Some("X".into()),
            storage_names: vec!["a".into()],
            ..Signals::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("maker"), "{json}");
        assert_eq!(serde_json::from_str::<Signals>(&json).unwrap(), s);
        assert_eq!(
            serde_json::from_str::<Signals>("{}").unwrap(),
            Signals::default()
        );
    }

    #[test]
    fn a_volume_id_never_counts_as_a_clash() {
        assert!(!can_clash(SignalKind::VolumeGuid));
        assert!(can_clash(SignalKind::VolumeSerial));
    }
}
