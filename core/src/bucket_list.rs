//! S3 mode's list of devices, worked out from the bucket (DESIGN.md section 11.7).
//!
//! Every device's folder is directly inside the base folder, so a PC that has the connection finds
//! them all by looking one level down ([`crate::store::find::discover`]). What the bucket says
//! about a device (its record: name, settings, what recognises it) is what counts. The list kept
//! on the PC is only a cache, to show something quickly and while the bucket is out of reach: a
//! device in it that the bucket doesn't show is kept, never dropped, except one that was forgotten.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use log::{info, warn};

use crate::owner;
use crate::paths::validate_component;
use crate::registry::{KnownDevice, Registry, is_plain_folder_name};
use crate::signals::DeviceKind;
use crate::store::dest::{directly_under, overlaps};
use crate::store::find::Discovered;
use crate::store::{Destination, S3Config};

/// Said about a device whose folder is not directly inside the base folder.
pub const NOT_UNDER_BASE: &str = "This device's folder isn't directly under the base folder, so other PCs won't find it. Use Change… to move it.";

/// A folder name under the base folder, as typed: one name (no `/`), with characters that are
/// safe wherever the folder may be listed. Returns it trimmed.
pub fn check_folder_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Give the folder a name.".into());
    }
    if n.contains('/') || n.contains('\\') {
        return Err(
            "The folder name can't have a / or \\ in it: the folder goes directly inside the base folder."
                .into(),
        );
    }
    if n.starts_with(".plug-n-keep") {
        return Err(
            "A folder name can't start with .plug-n-keep: the app keeps its own files under that name."
                .into(),
        );
    }
    validate_component(n)
        .map_err(|why| format!("The folder name \"{n}\" can't be used: it {why}."))?;
    Ok(n.to_string())
}

/// The place for the folder `name` directly inside the base folder `base`.
pub fn place_under(base: &S3Config, name: &str) -> Result<S3Config, String> {
    let name = check_folder_name(name)?;
    let b = crate::store::s3::normalise_prefix(&base.prefix);
    Ok(S3Config {
        prefix: format!("{b}{name}/"),
        ..base.clone()
    })
}

/// What [`apply`] did and found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Names of devices added to the list.
    pub added: Vec<String>,
    /// Names of devices already listed, whose place the bucket confirms.
    pub already_known: Vec<String>,
    /// Names of listed devices taken off because they were forgotten.
    pub dropped: Vec<String>,
    /// Things the user should hear about (two folders for one device, a folder that can't be
    /// used).
    pub problems: Vec<String>,
    /// A line for one device's page, by its ID.
    pub notes: Vec<(String, String)>,
}

/// Put what the bucket shows into `reg`. `found` is what was discovered under `base`;
/// `forgotten` the IDs the bucket's settings say were forgotten. `still_holds(place, id)` says
/// whether `place` still holds device `id`'s backup (asked only when the bucket shows a listed
/// device somewhere else).
///
/// - A device folder with a usable record gives the device as the record says (what this PC
///   learned about its signals is kept too).
/// - A folder with a marker but no usable record is shown with a note: an already listed device
///   keeps what this PC knows of it; a new one is made from the marker.
/// - One device ID in two folders is a problem line, and nothing is picked.
/// - A forgotten device is left out, and taken off the list if it was on it.
/// - Nothing listed is dropped because the bucket doesn't show it.
pub fn apply(
    reg: &mut Registry,
    base: &S3Config,
    found: &[Discovered],
    forgotten: &[String],
    now: &str,
    still_holds: &dyn Fn(&Destination, &str) -> bool,
) -> Applied {
    let mut out = Applied::default();
    let mut by_id: BTreeMap<&str, Vec<&Discovered>> = BTreeMap::new();
    for f in found {
        by_id.entry(&f.backup.device_id).or_default().push(f);
    }
    for (id, list) in by_id {
        if forgotten.iter().any(|f| f == id) {
            if let Some(d) = reg.forget(id) {
                info!(
                    "\"{}\" was forgotten on another PC, so it is off the list",
                    d.name
                );
                out.dropped.push(d.name);
            }
            continue;
        }
        if list.len() > 1 {
            let places: Vec<String> = list.iter().map(|f| format!("\"{}\"", f.name)).collect();
            let name = reg
                .get(id)
                .map_or_else(|| list[0].backup.device_name.clone(), |d| d.name.clone());
            let text = format!(
                "\"{name}\" was found in more than one folder of the bucket ({}). Nothing was changed: keep one of the folders, or take the .plug-n-keep.json file out of the others",
                places.join(", ")
            );
            warn!("{text}");
            out.problems.push(text);
            continue;
        }
        one(reg, list[0], now, still_holds, &mut out);
    }
    // A device forgotten on another PC that the bucket no longer shows is also taken off.
    for id in forgotten {
        if let Some(d) = reg.forget(id) {
            info!(
                "\"{}\" was forgotten on another PC, so it is off the list",
                d.name
            );
            out.dropped.push(d.name);
        }
    }
    out.notes.extend(not_under_base(reg, base));
    out
}

/// A note for each listed device whose folder in the bucket is not directly inside `base`. Such a
/// device keeps working, from this PC's list; no data is moved.
pub fn not_under_base(reg: &Registry, base: &S3Config) -> Vec<(String, String)> {
    reg.devices()
        .iter()
        .filter(|d| {
            matches!(d.destination(), Some(Destination::S3(c)) if directly_under(base, &c).is_none())
        })
        .map(|d| (d.id.clone(), NOT_UNDER_BASE.to_string()))
        .collect()
}

/// One device folder with a single claim on its ID.
fn one(
    reg: &mut Registry,
    f: &Discovered,
    now: &str,
    still_holds: &dyn Fn(&Destination, &str) -> bool,
    out: &mut Applied,
) {
    let b = &f.backup;
    let id = b.device_id.as_str();
    let mut dest = b.destination.clone();
    if let Destination::S3(c) = &mut dest {
        // The way every place in S3 mode is spelled: a folder ends in a slash.
        c.prefix = crate::store::s3::normalise_prefix(&c.prefix);
    }
    let problem_note = match (&b.device, &b.record_problem) {
        (_, Some(why)) => Some(format!(
            "The record of this device in the bucket can't be used ({why}). What is shown is what the marker and this PC know; the next change writes a new record."
        )),
        (None, None) => Some(
            "The bucket has only a marker for this device, no record yet. Its settings are the defaults until one is written."
                .to_string(),
        ),
        _ => None,
    };
    if let Some(local) = reg.get(id).cloned() {
        let local_dest = local.destination();
        let same = local_dest.as_ref().is_some_and(|l| l.same_place(&dest));
        if !same
            && let Some(l) = &local_dest
            && still_holds(l, id)
        {
            out.problems.push(format!(
                "\"{}\" has backups in {} and {}; pick one with Change… or Find it…",
                local.name,
                l.label().display(),
                dest.label().display()
            ));
            return;
        }
        let mut device = match &b.device {
            Some(rec) => {
                let mut d = rec.clone();
                d.history.merge(&local.history);
                d
            }
            None => local.clone(),
        };
        device.set_destination(dest);
        device.offline = None;
        if let Some(slot) = reg.get_mut(id) {
            *slot = device;
        }
        out.already_known.push(local.name);
        if let Some(t) = problem_note {
            out.notes.push((id.to_string(), t));
        }
        return;
    }
    if let Some(other) = reg
        .devices()
        .iter()
        .find(|d| d.destination().is_some_and(|o| overlaps(&o, &dest)))
    {
        out.problems.push(format!(
            "{} was not added: it overlaps the backup of \"{}\". Each device needs a folder of its own",
            dest.label().display(),
            other.name
        ));
        return;
    }
    let mut device = match &b.device {
        Some(rec) => rec.clone(),
        None => from_marker(&b.owner, &b.device_name, id, now),
    };
    device.set_destination(dest);
    device.offline = None;
    info!("Found the backup of \"{}\" in the bucket", device.name);
    out.added.push(device.name.clone());
    reg.add(device);
    if let Some(t) = problem_note {
        out.notes.push((id.to_string(), t));
    }
}

/// A device made from its folder's marker, for a folder with no usable record.
fn from_marker(m: &owner::FolderOwner, name: &str, id: &str, now: &str) -> KnownDevice {
    let kind = DeviceKind::guess_from(&m.signals);
    let mut d = KnownDevice::with_id(id.to_string(), kind, name, &m.signals, now);
    d.view = m.view;
    if let Some(n) = m.archive_names.last().filter(|n| is_plain_folder_name(n)) {
        d.settings.archive_name = n.clone();
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::folder_record;
    use crate::owner::FolderOwner;
    use crate::signals::{SignalKind, Signals, View};
    use crate::store::BackupStore;
    use crate::store::find::{Discovered, discover_with};
    use crate::store::mem::MemStore;

    const NOW: &str = "2026-01-01T00:00:00Z";

    fn base() -> S3Config {
        S3Config {
            provider: Default::default(),
            account_id: None,
            endpoint: "http://h:9000".into(),
            region: "us-east-1".into(),
            bucket: "b".into(),
            prefix: "devices/".into(),
            profile: None,
            credentials_file: None,
            path_style: true,
            ca_file: None,
        }
    }

    fn claim(store: &MemStore, id: &str, name: &str) {
        let me = FolderOwner::new(id, name, Signals::default(), View::WindowsWpd, NOW);
        owner::write_in(store, &me).unwrap();
    }

    fn record(store: &MemStore, id: &str, name: &str) {
        let mut d =
            KnownDevice::with_id(id.into(), DeviceKind::Ptp, name, &Signals::default(), NOW);
        d.settings.scan_automatically = true;
        folder_record::write_device_in(store, &d).unwrap();
    }

    /// A base folder with the given device folders, as the discovery sees it.
    fn discovered(kids: &[(&str, MemStore)]) -> Vec<Discovered> {
        let top = MemStore::new();
        for (n, _) in kids {
            top.insert(&format!("{n}/file.txt"), b"x", None);
        }
        let open = |d: &Destination| -> Result<Box<dyn BackupStore>, String> {
            let Destination::S3(c) = d else {
                return Err("S3 only".into());
            };
            let name = c
                .prefix
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap()
                .to_string();
            Ok(kids.iter().find(|(n, _)| *n == name).map_or_else(
                || Box::new(MemStore::new()) as _,
                |(_, s)| Box::new(s.clone()) as _,
            ))
        };
        discover_with(&top, &Destination::S3(base()), &open)
            .unwrap()
            .found
    }

    fn no_other_place(_: &Destination, _: &str) -> bool {
        false
    }

    #[test]
    fn names_under_the_base_are_one_safe_segment() {
        assert_eq!(check_folder_name("  Pixel 8 ").unwrap(), "Pixel 8");
        for bad in [
            "",
            "  ",
            "a/b",
            "a\\b",
            "..",
            ".",
            "a:b",
            "x.",
            ".plug-n-keep-x",
            "NUL",
            "a*",
        ] {
            assert!(check_folder_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            place_under(&base(), "Pixel").unwrap().prefix,
            "devices/Pixel/"
        );
        let mut top = base();
        top.prefix = String::new();
        assert_eq!(place_under(&top, "Pixel").unwrap().prefix, "Pixel/");
        assert!(place_under(&base(), "a/b").is_err());
    }

    #[test]
    fn a_new_pc_gets_every_device_from_the_bucket_with_its_settings() {
        let (a, b) = (MemStore::new(), MemStore::new());
        claim(&a, "dev-a", "Phone A");
        record(&a, "dev-a", "Phone A (record)");
        claim(&b, "dev-b", "Phone B");
        record(&b, "dev-b", "Phone B");
        let found = discovered(&[("A", a), ("B", b), ("empty", MemStore::new())]);
        assert_eq!(found.len(), 2);
        let mut reg = Registry::default();
        let r = apply(&mut reg, &base(), &found, &[], NOW, &no_other_place);
        assert_eq!(r.added, ["Phone A (record)", "Phone B"]);
        assert!(r.problems.is_empty() && r.notes.is_empty(), "{r:?}");
        let d = reg.get("dev-a").unwrap();
        assert!(d.settings.scan_automatically, "the bucket's record counts");
        let Some(Destination::S3(c)) = d.destination() else {
            panic!("a bucket place");
        };
        assert_eq!(c.prefix, "devices/A/");
        // Again: nothing is added twice.
        let again = apply(&mut reg, &base(), &found, &[], NOW, &no_other_place);
        assert!(again.added.is_empty());
        assert_eq!(again.already_known.len(), 2);
        assert_eq!(reg.devices().len(), 2);
    }

    #[test]
    fn the_buckets_record_wins_over_the_cache_and_keeps_what_this_pc_learned() {
        let a = MemStore::new();
        claim(&a, "dev-a", "Phone A");
        record(&a, "dev-a", "Renamed elsewhere");
        let found = discovered(&[("A", a)]);
        let mut cached = KnownDevice::with_id(
            "dev-a".into(),
            DeviceKind::Ptp,
            "Old cached name",
            &Signals {
                device_serial: Some("SERIAL1".into()),
                ..Signals::default()
            },
            NOW,
        );
        cached.set_destination(Destination::S3(place_under(&base(), "A").unwrap()));
        let mut reg = Registry::new(vec![cached]);
        let r = apply(&mut reg, &base(), &found, &[], NOW, &no_other_place);
        assert_eq!(r.already_known, ["Old cached name"]);
        let d = reg.get("dev-a").unwrap();
        assert_eq!(d.name, "Renamed elsewhere");
        assert!(d.settings.scan_automatically);
        assert!(
            d.history.contains(SignalKind::DeviceSerial, "serial1")
                || d.history.contains(SignalKind::DeviceSerial, "SERIAL1"),
            "the signals learned here are kept"
        );
    }

    #[test]
    fn a_forgotten_device_is_left_out_and_taken_off_a_list_that_has_it() {
        let a = MemStore::new();
        claim(&a, "dev-a", "Phone A");
        record(&a, "dev-a", "Phone A");
        let found = discovered(&[("A", a)]);
        let forgotten = vec!["dev-a".to_string()];
        let mut empty = Registry::default();
        let r = apply(
            &mut empty,
            &base(),
            &found,
            &forgotten,
            NOW,
            &no_other_place,
        );
        assert!(r.added.is_empty() && empty.devices().is_empty());
        let mut reg = Registry::default();
        apply(&mut reg, &base(), &found, &[], NOW, &no_other_place);
        assert_eq!(reg.devices().len(), 1);
        let r = apply(&mut reg, &base(), &found, &forgotten, NOW, &no_other_place);
        assert_eq!(r.dropped, ["Phone A"]);
        assert!(reg.devices().is_empty());
    }

    #[test]
    fn a_device_the_bucket_does_not_show_stays_in_the_cache() {
        let mut d = KnownDevice::with_id(
            "dev-x".into(),
            DeviceKind::Ptp,
            "Cached",
            &Signals::default(),
            NOW,
        );
        d.set_destination(Destination::S3(place_under(&base(), "X").unwrap()));
        let mut reg = Registry::new(vec![d]);
        let r = apply(&mut reg, &base(), &[], &[], NOW, &no_other_place);
        assert_eq!(reg.devices().len(), 1);
        assert!(
            r.notes.is_empty(),
            "it is directly under the base: nothing to say"
        );
    }

    #[test]
    fn a_missing_or_damaged_record_is_shown_with_the_problem() {
        let (a, b) = (MemStore::new(), MemStore::new());
        claim(&a, "dev-a", "Marker only");
        claim(&b, "dev-b", "Damaged record");
        b.insert(folder_record::DEVICE_FILE, b"not json", None);
        let found = discovered(&[("A", a), ("B", b)]);
        let mut reg = Registry::default();
        let r = apply(&mut reg, &base(), &found, &[], NOW, &no_other_place);
        assert_eq!(r.added, ["Marker only", "Damaged record"]);
        assert_eq!(r.notes.len(), 2);
        let text = |id: &str| r.notes.iter().find(|(i, _)| i == id).unwrap().1.clone();
        assert!(text("dev-a").contains("only a marker"));
        assert!(text("dev-b").contains("can't be used"));
    }

    #[test]
    fn one_device_in_two_folders_is_a_problem_and_nothing_is_picked() {
        let (a, b) = (MemStore::new(), MemStore::new());
        claim(&a, "dev-a", "Phone A");
        record(&a, "dev-a", "Phone A");
        claim(&b, "dev-a", "Phone A");
        record(&b, "dev-a", "Phone A");
        let found = discovered(&[("A", a), ("A copy", b)]);
        let mut reg = Registry::default();
        let r = apply(&mut reg, &base(), &found, &[], NOW, &no_other_place);
        assert!(reg.devices().is_empty());
        assert_eq!(r.problems.len(), 1);
        assert!(
            r.problems[0].contains("more than one folder"),
            "{:?}",
            r.problems
        );
    }

    #[test]
    fn a_listed_device_found_elsewhere_is_a_problem_only_if_its_old_place_still_holds_it() {
        let a = MemStore::new();
        claim(&a, "dev-a", "Phone A");
        record(&a, "dev-a", "Phone A");
        let found = discovered(&[("A", a)]);
        let mk = || {
            let mut d = KnownDevice::with_id(
                "dev-a".into(),
                DeviceKind::Ptp,
                "Phone A",
                &Signals::default(),
                NOW,
            );
            let mut old = base();
            old.prefix = "elsewhere/phone/".into();
            d.set_destination(Destination::S3(old));
            Registry::new(vec![d])
        };
        // The old place still holds it: two places, nothing changes.
        let mut reg = mk();
        let r = apply(&mut reg, &base(), &found, &[], NOW, &|_, _| true);
        assert_eq!(r.problems.len(), 1);
        let Some(Destination::S3(c)) = reg.get("dev-a").unwrap().destination() else {
            panic!()
        };
        assert_eq!(c.prefix, "elsewhere/phone/");
        assert_eq!(r.notes.len(), 1, "and the old place is not under the base");
        // It is gone from there: the device follows the bucket.
        let mut reg = mk();
        let r = apply(&mut reg, &base(), &found, &[], NOW, &|_, _| false);
        assert!(r.problems.is_empty());
        let Some(Destination::S3(c)) = reg.get("dev-a").unwrap().destination() else {
            panic!()
        };
        assert_eq!(c.prefix, "devices/A/");
    }

    #[test]
    fn a_legacy_device_not_under_the_base_keeps_working_with_a_note() {
        let mut d = KnownDevice::with_id(
            "old".into(),
            DeviceKind::Ptp,
            "Legacy",
            &Signals::default(),
            NOW,
        );
        let mut elsewhere = base();
        elsewhere.prefix = "phones/legacy/".into();
        d.set_destination(Destination::S3(elsewhere));
        let mut at_base = KnownDevice::with_id(
            "top".into(),
            DeviceKind::Ptp,
            "AtBase",
            &Signals::default(),
            NOW,
        );
        at_base.set_destination(Destination::S3(base()));
        let mut reg = Registry::new(vec![d, at_base]);
        let r = apply(&mut reg, &base(), &[], &[], NOW, &no_other_place);
        let ids: Vec<&str> = r.notes.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(ids, ["old", "top"]);
        assert!(
            r.notes[0]
                .1
                .contains("isn't directly under the base folder")
        );
        assert_eq!(reg.devices().len(), 2);
    }

    #[test]
    fn the_base_folders_own_marker_makes_no_device() {
        let top = MemStore::new();
        claim(&top, "dev-base", "Base");
        top.insert("kid/x", b"x", None);
        let kid = MemStore::new();
        let open =
            |_: &Destination| -> Result<Box<dyn BackupStore>, String> { Ok(Box::new(kid.clone())) };
        let d = discover_with(&top, &Destination::S3(base()), &open).unwrap();
        assert!(d.found.is_empty() && d.problems.is_empty());
    }
}
