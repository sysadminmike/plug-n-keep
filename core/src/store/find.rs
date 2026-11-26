//! "Find my backups" and "Add an existing backup" for a bucket (S3-PLAN section 1): look for an
//! owner marker at the place itself, else one level below it (a folder list with `/` as the
//! delimiter on an object store, one directory level on a folder). Only reads.
#![forbid(unsafe_code)]

use log::warn;

use super::{BackupStore, Destination, explain, is_offline};
use crate::folder_record;
use crate::owner::{self, FolderOwner, OwnerState};
use crate::paths::SafeRelPath;
use crate::registry::KnownDevice;

/// Most folders one level below that are looked into; a bucket with more is not a backup place.
const MAX_FOLDERS: usize = 500;

/// How a place below is opened.
type OpenFn<'a> = dyn Fn(&Destination) -> Result<Box<dyn BackupStore>, String> + 'a;

/// One backup found.
#[derive(Debug, Clone)]
pub struct FoundBackup {
    /// Where it is (the place itself or the folder below it).
    pub destination: Destination,
    pub device_id: String,
    /// The name the device record holds, else the marker's.
    pub device_name: String,
    /// What the marker says.
    pub owner: FolderOwner,
    /// The device record kept with the backup, if there is a readable one. Its `backup_dir` is
    /// not set (see [`folder_record::read_device_in`]).
    pub device: Option<KnownDevice>,
    /// Why the device record could not be used, if it is there and damaged.
    pub record_problem: Option<String>,
}

/// The backups at `dest`: the one there if `dest` has a marker, else those one level below.
/// `Err` (plain words, "No network: ..." when it can't be reached) if the place can't be read.
pub fn find_backups(dest: &Destination) -> Result<Vec<FoundBackup>, String> {
    let top = dest.open()?;
    find_with(&*top, dest, &|d| d.open())
}

/// [`find_backups`] with the way to open the folders below given (tests use other stores).
pub(crate) fn find_with(
    top: &dyn BackupStore,
    dest: &Destination,
    open: &OpenFn<'_>,
) -> Result<Vec<FoundBackup>, String> {
    let at_top = marker_state(top).map_err(|e| explain(&e))?;
    match at_top {
        Some(Ok(_)) => return Ok(found_at(top, dest).into_iter().collect()),
        Some(Err(why)) => return Err(format!("the owner marker here can't be read ({why})")),
        None => {}
    }
    let mut names = top.list_dirs("").map_err(|e| explain(&e))?;
    if names.len() > MAX_FOLDERS {
        warn!(
            "{} has {} folders; only the first {MAX_FOLDERS} are looked into",
            top.describe(),
            names.len()
        );
        names.truncate(MAX_FOLDERS);
    }
    let mut out = Vec::new();
    for name in names {
        let child = child_destination(dest, &name);
        let store = match open(&child) {
            Ok(s) => s,
            Err(e) => {
                warn!("Could not look into {}: {e}", child.label().display());
                continue;
            }
        };
        match marker_state(&*store) {
            Ok(Some(Ok(_))) => out.extend(found_at(&*store, &child)),
            Ok(Some(Err(why))) => warn!(
                "The owner marker in {} can't be read ({why}); left out",
                child.label().display()
            ),
            Ok(None) => {}
            // The place went away part-way: say so rather than a short list.
            Err(e) if is_offline(&e) => return Err(explain(&e)),
            Err(e) => warn!("Could not look into {}: {e}", child.label().display()),
        }
    }
    Ok(out)
}

/// `None`: no marker. `Some(Ok(()))`: one is there. `Some(Err(why))`: it can't be read.
/// `Err`: the store could not be asked.
fn marker_state(store: &dyn BackupStore) -> Result<Option<Result<(), String>>, std::io::Error> {
    let rel = SafeRelPath::parse(owner::MARKER).expect("the marker name is a safe name");
    match store.stat(&rel)? {
        None => Ok(None),
        Some(_) => Ok(Some(match owner::read_in(store) {
            OwnerState::Owned(_) => Ok(()),
            OwnerState::Unclaimed => return Ok(None),
            OwnerState::Unreadable(e) => Err(e),
        })),
    }
}

/// The backup held by `store` (known to have a readable marker), if its device ID is usable.
fn found_at(store: &dyn BackupStore, dest: &Destination) -> Option<FoundBackup> {
    let OwnerState::Owned(owner) = owner::read_in(store) else {
        return None;
    };
    if !crate::registry::is_sane_device_id(&owner.device_id) {
        warn!(
            "The owner marker in {} has no usable device ID; left out",
            dest.label().display()
        );
        return None;
    }
    let (device, record_problem) = match folder_record::read_device_in(store, &owner.device_id) {
        Ok(d) => (d, None),
        Err(why) => (None, Some(why)),
    };
    let device_name = device
        .as_ref()
        .map_or_else(|| owner.device_name.clone(), |d| d.name.clone());
    Some(FoundBackup {
        destination: dest.clone(),
        device_id: owner.device_id.clone(),
        device_name,
        owner: *owner,
        device,
        record_problem,
    })
}

/// One device folder found directly under a base folder.
#[derive(Debug, Clone)]
pub struct Discovered {
    /// The folder's name under the base.
    pub name: String,
    pub backup: FoundBackup,
}

/// What [`discover`] found.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub found: Vec<Discovered>,
    /// Folders with a marker that can't be used.
    pub problems: Vec<String>,
}

/// The device folders directly under the base folder `base` (S3 mode's device list): each folder
/// with a readable owner marker. The base itself is never a device, even if it has a marker.
/// `Err` (plain words, "No network: ..." when it can't be reached) if it can't be read.
pub fn discover(base: &Destination) -> Result<Discovery, String> {
    let top = base.open()?;
    discover_in(&*top, base)
}

/// [`discover`] with the store of the base folder already open.
pub fn discover_in(top: &dyn BackupStore, base: &Destination) -> Result<Discovery, String> {
    discover_with(top, base, &|d| d.open())
}

/// [`discover`] with the way to open the folders below given (tests use other stores).
pub(crate) fn discover_with(
    top: &dyn BackupStore,
    base: &Destination,
    open: &OpenFn<'_>,
) -> Result<Discovery, String> {
    let mut names = top.list_dirs("").map_err(|e| explain(&e))?;
    if names.len() > MAX_FOLDERS {
        warn!(
            "{} has {} folders; only the first {MAX_FOLDERS} are looked into",
            top.describe(),
            names.len()
        );
        names.truncate(MAX_FOLDERS);
    }
    let mut out = Discovery::default();
    for name in names {
        let child = child_destination(base, &name);
        let label = child.label().display().to_string();
        let store = match open(&child) {
            Ok(s) => s,
            Err(e) => {
                warn!("Could not look into {label}: {e}");
                continue;
            }
        };
        match marker_state(&*store) {
            Ok(Some(Ok(_))) => match found_at(&*store, &child) {
                Some(backup) => out.found.push(Discovered { name, backup }),
                None => out.problems.push(format!(
                    "The owner marker in {label} has no usable device ID"
                )),
            },
            Ok(Some(Err(why))) => out
                .problems
                .push(format!("The owner marker in {label} can't be read ({why})")),
            Ok(None) => {}
            Err(e) if is_offline(&e) => return Err(explain(&e)),
            Err(e) => warn!("Could not look into {label}: {e}"),
        }
    }
    Ok(out)
}

/// The place for the folder `name` directly inside `dest`.
pub fn child_destination(dest: &Destination, name: &str) -> Destination {
    match dest {
        Destination::Local(p) => Destination::Local(p.join(name)),
        Destination::S3(c) => {
            let mut c = c.clone();
            c.prefix = format!("{}{name}", super::s3::normalise_prefix(&c.prefix));
            Destination::S3(c)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::signals::{DeviceKind, Signals, View};
    use crate::store::S3Config;
    use crate::store::mem::MemStore;
    use crate::store::s3::S3Store;
    use crate::store::s3::fake::FakeS3;

    fn claim(store: &dyn BackupStore, id: &str, name: &str) {
        let me = FolderOwner::new(
            id,
            name,
            Signals::default(),
            View::WindowsWpd,
            "2026-01-01T00:00:00Z",
        );
        owner::write_in(store, &me).unwrap();
    }

    fn record(store: &dyn BackupStore, id: &str, name: &str) {
        let d = KnownDevice::with_id(
            id.into(),
            DeviceKind::Mtp,
            name,
            &Signals::default(),
            "2026-01-01T00:00:00Z",
        );
        folder_record::write_device_in(store, &d).unwrap();
    }

    fn here() -> Destination {
        Destination::Local(PathBuf::from("mem-root"))
    }

    #[test]
    fn a_marker_at_the_place_itself_is_the_one_backup() {
        let top = MemStore::new();
        claim(&top, "dev-1", "Marker name");
        record(&top, "dev-1", "Record name");
        // A folder below is not looked at once the place has a marker of its own.
        top.insert("kid/x.txt", b"x", None);
        let found = find_with(&top, &here(), &|_| Err("not opened".into())).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].device_id, "dev-1");
        assert_eq!(found[0].device_name, "Record name", "the record wins");
        assert!(found[0].device.is_some() && found[0].record_problem.is_none());
        assert_eq!(found[0].destination, here());
    }

    #[test]
    fn markers_one_level_below_are_found_and_the_rest_is_left_out() {
        let top = MemStore::new();
        let kids: Vec<(&str, MemStore)> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|n| (*n, MemStore::new()))
            .collect();
        for (n, _) in &kids {
            top.insert(&format!("{n}/file.txt"), b"x", None);
        }
        claim(&kids[0].1, "dev-a", "Phone A");
        record(&kids[0].1, "dev-a", "Phone A");
        // No record: the marker's name is used.
        claim(&kids[1].1, "dev-b", "Phone B");
        // A marker with an unusable ID, a damaged marker, and no marker at all.
        claim(&kids[2].1, "has a space", "Bad id");
        kids[3].1.insert(owner::MARKER, b"not json", None);
        // A marker two levels down is not looked for.
        top.insert("deep/er/m/x", b"x", None);
        let open = |d: &Destination| -> Result<Box<dyn BackupStore>, String> {
            let Destination::Local(p) = d else {
                return Err("local only".into());
            };
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            Ok(kids.iter().find(|(n, _)| *n == name).map_or_else(
                || Box::new(MemStore::new()) as _,
                |(_, s)| Box::new(s.clone()) as _,
            ))
        };
        let found = find_with(&top, &here(), &open).unwrap();
        let ids: Vec<&str> = found.iter().map(|f| f.device_id.as_str()).collect();
        assert_eq!(ids, ["dev-a", "dev-b"]);
        assert_eq!(found[0].destination, child_destination(&here(), "a"));
        assert_eq!(found[1].device_name, "Phone B");
        assert!(found[1].device.is_none());
    }

    #[test]
    fn an_unreachable_place_is_an_error_not_an_empty_list() {
        let top = MemStore::new();
        top.fail_nth_offline("stat", 1);
        let e = find_with(&top, &here(), &|_| Err("x".into())).unwrap_err();
        assert!(e.starts_with("No network"), "{e}");
        // Going offline part-way through the folders is an error too.
        let top = MemStore::new();
        top.insert("a/x", b"x", None);
        let kid = MemStore::new();
        kid.fail_nth_offline("stat", 1);
        let e = find_with(&top, &here(), &|_| Ok(Box::new(kid.clone()))).unwrap_err();
        assert!(e.starts_with("No network"), "{e}");
    }

    #[test]
    fn child_places_are_named_inside_the_place() {
        let cfg = S3Config {
            provider: Default::default(),
            account_id: None,
            endpoint: "http://h".into(),
            region: "r".into(),
            bucket: "b".into(),
            prefix: "/phones".into(),
            profile: None,
            credentials_file: None,
            path_style: true,
            ca_file: None,
        };
        let child = |c: S3Config, n: &str| match child_destination(&Destination::S3(c), n) {
            Destination::S3(c) => c.prefix,
            Destination::Local(_) => panic!("an S3 place"),
        };
        assert_eq!(child(cfg.clone(), "pixel"), "phones/pixel");
        assert_eq!(
            child(
                S3Config {
                    provider: Default::default(),
                    account_id: None,
                    prefix: String::new(),
                    ..cfg
                },
                "x"
            ),
            "x"
        );
    }

    /// The bucket listing with a delimiter, on the pretend server.
    #[test]
    fn a_bucket_is_searched_one_level_down_on_the_pretend_server() {
        let (_creds, cfg) = crate::store::s3::tests::offline_config();
        let fake = FakeS3::new();
        let top = S3Store::on_fake_with(cfg.clone(), &fake);
        let dest = Destination::S3(cfg);
        let open = |d: &Destination| -> Result<Box<dyn BackupStore>, String> {
            let Destination::S3(c) = d else {
                return Err("S3 only".into());
            };
            Ok(Box::new(S3Store::on_fake_with(c.clone(), &fake)))
        };
        // Nothing in the bucket yet.
        assert!(find_with(&top, &dest, &open).unwrap().is_empty());
        for (n, id) in [("pixel", "dev-p"), ("iphone", "dev-i")] {
            let child = open(&child_destination(&dest, n)).unwrap();
            claim(&*child, id, n);
            record(&*child, id, &format!("{n} record"));
        }
        // A folder with files but no marker, and a file of the place's own.
        fake.insert("p/other/file.txt", b"x", &[]);
        fake.insert("p/loose.txt", b"x", &[]);
        assert_eq!(top.list_dirs("").unwrap(), ["iphone", "other", "pixel"]);
        let found = find_with(&top, &dest, &open).unwrap();
        let got: Vec<(&str, &str)> = found
            .iter()
            .map(|f| (f.device_id.as_str(), f.device_name.as_str()))
            .collect();
        assert_eq!(got, [("dev-i", "iphone record"), ("dev-p", "pixel record")]);
        let Destination::S3(c) = &found[0].destination else {
            panic!("S3");
        };
        assert_eq!(c.prefix, "p/iphone");
        // With a marker at the place itself, that one backup is the answer.
        claim(&top, "dev-top", "Top");
        let found = find_with(&top, &dest, &open).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].device_id, "dev-top");
    }

    /// Against the real bucket (`PNK_S3_TEST=1`): the same search through `find_backups`.
    #[test]
    fn a_real_bucket_is_searched_one_level_down() {
        let Some(f) = crate::store::s3::tests::fixture() else {
            return;
        };
        let dest = Destination::S3(f.config().clone());
        assert!(find_backups(&dest).unwrap().is_empty());
        for (n, id) in [("pixel", "dev-p"), ("iphone", "dev-i")] {
            let child = child_destination(&dest, n).open().unwrap();
            claim(&*child, id, n);
            record(&*child, id, &format!("{n} record"));
        }
        let other = child_destination(&dest, "other").open().unwrap();
        other
            .write_new(&crate::paths::SafeRelPath::parse("f.txt").unwrap(), b"x")
            .unwrap();
        let found = find_backups(&dest).unwrap();
        let got: Vec<&str> = found.iter().map(|f| f.device_name.as_str()).collect();
        assert_eq!(got, ["iphone record", "pixel record"]);
        // The place itself holding a marker is the one backup.
        let me = child_destination(&dest, "pixel");
        let found = find_backups(&me).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].destination.same_place(&me));
    }
}
