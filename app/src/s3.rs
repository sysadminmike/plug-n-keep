//! A backup in a bucket, as the window deals with it: the fields the user types, what the Test
//! button says, how a bucket's state is worked out, and the check that two devices don't share
//! a place. No window in here, and no keys: the profile name and an optional file path are all
//! the app keeps.

use std::io;
use std::path::PathBuf;

use pnk_core::owner::OwnerState;
use pnk_core::state::Config;
use pnk_core::store::s3::provider::Provider;
use pnk_core::store::{self, BackupStore, Destination, S3Config};

use crate::worker::FolderState;

/// The S3 fields as they are typed. Text until [`S3Form::config`] checks them. Which fields are
/// shown depends on the provider ([`main_fields`], [`more_fields`]); the rest are not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Form {
    pub provider: Provider,
    /// Amazon S3, Wasabi and Other.
    pub region: String,
    /// Cloudflare R2.
    pub account_id: String,
    /// Backblaze B2's endpoint, or the server address of Other.
    pub server: String,
    pub bucket: String,
    /// The folder in the bucket.
    pub prefix: String,
    pub profile: String,
    pub credentials_file: String,
    /// Other only.
    pub path_style: bool,
    /// Other only.
    pub ca_file: String,
}

impl Default for S3Form {
    fn default() -> Self {
        Self {
            provider: Provider::Other,
            region: "us-east-1".into(),
            account_id: String::new(),
            server: String::new(),
            bucket: String::new(),
            prefix: String::new(),
            profile: "default".into(),
            credentials_file: String::new(),
            path_style: true,
            ca_file: String::new(),
        }
    }
}

/// A field a provider shows, named as the provider's own console names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fld {
    Region,
    AccountId,
    Endpoint,
    Server,
    Bucket,
}

impl Fld {
    pub fn label(self) -> &'static str {
        match self {
            Fld::Region => "Region",
            Fld::AccountId => "Account ID",
            Fld::Endpoint => "Endpoint",
            Fld::Server => "Server address",
            Fld::Bucket => "Bucket",
        }
    }
}

/// The fields shown for a provider, before Base folder, Profile and Credentials file.
pub fn main_fields(p: Provider) -> &'static [Fld] {
    match p {
        Provider::Aws | Provider::Wasabi => &[Fld::Region, Fld::Bucket],
        Provider::R2 => &[Fld::AccountId, Fld::Bucket],
        Provider::B2 => &[Fld::Endpoint, Fld::Bucket],
        Provider::Other => &[Fld::Server, Fld::Bucket],
    }
}

/// The fields kept under "More": only a server of your own has any.
pub fn more_fields(p: Provider) -> &'static [&'static str] {
    match p {
        Provider::Other => &["Region", "Path-style addressing", "CA file"],
        _ => &[],
    }
}

impl S3Form {
    pub fn from_config(c: &S3Config) -> Self {
        Self {
            provider: c.provider,
            region: c.region.clone(),
            account_id: c.account_id.clone().unwrap_or_default(),
            server: c.endpoint.clone(),
            bucket: c.bucket.clone(),
            prefix: c.prefix.clone(),
            profile: c.profile.clone().unwrap_or_default(),
            credentials_file: path_text(c.credentials_file.as_ref()),
            path_style: c.path_style,
            ca_file: path_text(c.ca_file.as_ref()),
        }
    }

    /// The settings these fields say, or what is wrong with them. The endpoint, region and
    /// addressing come from the provider's own fields.
    pub fn config(&self) -> Result<S3Config, String> {
        let (endpoint, region, path_style, ca_file) = match self.provider {
            Provider::Other => {
                let endpoint = self.server.trim().trim_end_matches('/');
                if endpoint.is_empty() {
                    return Err(
                        "Enter the S3 server's address, such as http://s3.example.com:9000.".into(),
                    );
                }
                if !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
                    || endpoint.len() < 9
                {
                    return Err("The server's address must start with http:// or https://.".into());
                }
                let region = self.region.trim();
                if region.is_empty() {
                    return Err("Enter the region, such as us-east-1.".into());
                }
                (
                    endpoint.to_string(),
                    region.to_string(),
                    self.path_style,
                    optional_path(&self.ca_file),
                )
            }
            p => {
                let d = p.derive(&self.region, &self.account_id, &self.server)?;
                (d.endpoint, d.region, d.path_style, None)
            }
        };
        let authority = endpoint
            .split_once("://")
            .map_or(endpoint.as_str(), |(_, rest)| rest)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if authority.contains('@') || endpoint.contains('?') || endpoint.contains('#') {
            return Err(
                "The server's address can't have a user name, password, ? or # in it. Put keys in the credentials file, not in the server address."
                    .into(),
            );
        }
        let bucket = self.bucket.trim();
        if bucket.is_empty() {
            return Err("Enter the bucket's name.".into());
        }
        if bucket.contains('/') || bucket.contains(char::is_whitespace) {
            return Err("The bucket's name can't have a / or spaces in it.".into());
        }
        let prefix = self.prefix.trim().to_string();
        check_folder(&prefix)?;
        let profile = self.profile.trim();
        Ok(S3Config {
            provider: self.provider,
            account_id: (self.provider == Provider::R2).then(|| self.account_id.trim().to_string()),
            endpoint,
            region,
            bucket: bucket.to_string(),
            prefix,
            profile: (!profile.is_empty()).then(|| profile.to_string()),
            credentials_file: optional_path(&self.credentials_file),
            path_style,
            ca_file,
        })
    }

    /// The same, for the S3 settings: nothing typed at all means no S3 server set up yet.
    pub fn default_config(&self) -> Result<Option<S3Config>, String> {
        if self.server.trim().is_empty()
            && self.account_id.trim().is_empty()
            && self.bucket.trim().is_empty()
        {
            return Ok(None);
        }
        self.config().map(Some)
    }
}

fn path_text(p: Option<&PathBuf>) -> String {
    p.map(|p| p.display().to_string()).unwrap_or_default()
}

fn optional_path(text: &str) -> Option<PathBuf> {
    let t = text.trim();
    (!t.is_empty()).then(|| PathBuf::from(t))
}

/// `s3://bucket/folder/`, for showing and copying.
pub fn address(c: &S3Config) -> String {
    Destination::S3(c.clone()).label().display().to_string()
}

/// The same rule the store applies to the folder in the bucket, so a bad one is said at once.
fn check_folder(p: &str) -> Result<(), String> {
    let t = p.trim().trim_matches('/');
    if t.is_empty() {
        return Ok(());
    }
    if t.contains('\\') || t.chars().any(char::is_control) {
        return Err(format!(
            "The folder \"{p}\" can't be used: no backslashes or control characters."
        ));
    }
    if t.split('/').any(|seg| matches!(seg, "" | "." | "..")) {
        return Err(format!(
            "The folder \"{p}\" can't be used: no empty, \".\" or \"..\" parts."
        ));
    }
    Ok(())
}

/// The other known device (not `own`) whose backup place overlaps `dest`, if one does.
pub fn overlapping_device<'a>(
    known: &'a [pnk_core::registry::KnownDevice],
    own: Option<&str>,
    dest: &Destination,
) -> Option<&'a pnk_core::registry::KnownDevice> {
    known
        .iter()
        .filter(|d| Some(d.id.as_str()) != own)
        .find(|d| {
            d.destination()
                .is_some_and(|o| pnk_core::store::dest::overlaps(&o, dest))
        })
}

/// What to tell the user when a place overlaps another device's.
pub fn overlap_text(other_name: &str) -> String {
    format!(
        "That place overlaps the backup of \"{other_name}\". Each device needs a folder of its own, with no folder inside another's."
    )
}

/// What Test (and a check of a device's bucket) found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    Ok,
    /// Works, with something worth saying about the server.
    OkWithNote(String),
    NoNetwork(String),
    NoAccess,
    BucketNotFound,
    ServerUnsupported(String),
    NoCredentials(String),
    /// The fields themselves are not usable (nothing was sent anywhere).
    BadSettings(String),
    Other(String),
}

impl TestOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, TestOutcome::Ok | TestOutcome::OkWithNote(_))
    }

    /// For the window. Never holds a key: the messages only name the profile and the file.
    pub fn text(&self) -> String {
        match self {
            TestOutcome::Ok => "OK: the bucket can be used.".into(),
            TestOutcome::OkWithNote(n) => format!("OK: the bucket can be used. {n}"),
            TestOutcome::NoNetwork(why) => format!("No network: {why}"),
            TestOutcome::NoAccess => {
                "No access: the server refused these credentials for this bucket.".into()
            }
            TestOutcome::BucketNotFound => "Bucket not found on this server.".into(),
            TestOutcome::ServerUnsupported(why) => format!("This server can't be used: {why}"),
            TestOutcome::NoCredentials(why) => format!("Credentials not found: {why}"),
            TestOutcome::BadSettings(why) => why.clone(),
            TestOutcome::Other(why) => format!("Could not use the bucket: {why}"),
        }
    }
}

/// The answer to "open the store" failing: a missing profile or file is told apart from a
/// setting that can't be used.
pub fn open_failure(message: &str) -> TestOutcome {
    let m = message.to_lowercase();
    if m.contains("credentials") || m.starts_with("profile ") {
        TestOutcome::NoCredentials(message.to_string())
    } else {
        TestOutcome::Other(message.to_string())
    }
}

/// What `root_exists` (which runs the server's self-test) says, as the Test button reports it.
pub fn test_outcome(root: &io::Result<bool>) -> TestOutcome {
    match root {
        Ok(true) => TestOutcome::Ok,
        Ok(false) => TestOutcome::BucketNotFound,
        Err(e) if store::is_offline(e) => TestOutcome::NoNetwork(e.to_string()),
        Err(e) => match e.kind() {
            io::ErrorKind::PermissionDenied => TestOutcome::NoAccess,
            io::ErrorKind::Unsupported => TestOutcome::ServerUnsupported(e.to_string()),
            io::ErrorKind::NotFound => TestOutcome::BucketNotFound,
            _ => TestOutcome::Other(e.to_string()),
        },
    }
}

/// Where a device's bucket stands, from what `root_exists` said and whether its folder in the
/// bucket still has the backup's marker. `no_marker_after_sync`: the device has synced before
/// but there is no marker now. Gives the state and a line for the window.
pub fn s3_folder_state(
    root: &io::Result<bool>,
    no_marker_after_sync: bool,
) -> (FolderState, String) {
    match root {
        Ok(true) if no_marker_after_sync => (
            FolderState::Missing,
            "Nothing of this backup is at that place any more.".into(),
        ),
        Ok(true) => (FolderState::Present, String::new()),
        Ok(false) => (FolderState::Missing, "The bucket was not found.".into()),
        Err(e) if store::is_offline(e) => (FolderState::Offline, store::explain(e)),
        Err(e) => match e.kind() {
            io::ErrorKind::PermissionDenied => (
                FolderState::NoAccess,
                "No access: the server refused these credentials for this bucket.".into(),
            ),
            io::ErrorKind::Unsupported => (FolderState::ServerUnsupported, e.to_string()),
            io::ErrorKind::NotFound => (FolderState::Missing, "The bucket was not found.".into()),
            _ => (FolderState::Unusable, e.to_string()),
        },
    }
}

/// Looks at a device's bucket: reachable, allowed, supported, and still holding the backup.
/// Reads only (the server's self-test writes and removes a small probe object).
pub fn check_store(store: &dyn BackupStore, had_sync: bool) -> (FolderState, String) {
    let root = store.root_exists();
    let no_marker = had_sync
        && matches!(root, Ok(true))
        && matches!(pnk_core::owner::read_in(store), OwnerState::Unclaimed);
    s3_folder_state(&root, no_marker)
}

/// The S3 settings page starts from this: what the S3 mode's settings hold.
pub fn default_form(config: &Config) -> S3Form {
    config
        .default_s3
        .as_ref()
        .map_or_else(S3Form::default, S3Form::from_config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(endpoint: &str, bucket: &str, prefix: &str) -> S3Config {
        S3Config {
            provider: Default::default(),
            account_id: None,
            endpoint: endpoint.into(),
            region: "us-east-1".into(),
            bucket: bucket.into(),
            prefix: prefix.into(),
            profile: None,
            credentials_file: None,
            path_style: true,
            ca_file: None,
        }
    }

    fn ov(a: &S3Config, b: &S3Config) -> bool {
        pnk_core::store::dest::overlaps(&Destination::S3(a.clone()), &Destination::S3(b.clone()))
    }

    #[test]
    fn places_overlap_only_inside_the_same_bucket_on_a_slash_boundary() {
        let a = cfg("http://h:9000", "b", "phones/a/");
        assert!(ov(&a, &cfg("http://h:9000", "b", "phones/a")));
        assert!(ov(&a, &cfg("http://h:9000", "b", "phones/")));
        assert!(ov(&a, &cfg("http://h:9000", "b", "phones/a/DCIM/")));
        assert!(ov(&a, &cfg("http://h:9000", "b", "")));
        // Only sharing a start is not overlapping.
        assert!(!ov(&a, &cfg("http://h:9000", "b", "phones/ab/")));
        assert!(!ov(&a, &cfg("http://h:9000", "b", "phones/b/")));
        // Another bucket or server is another place.
        assert!(!ov(&a, &cfg("http://h:9000", "c", "phones/a/")));
        assert!(!ov(&a, &cfg("http://other:9000", "b", "phones/a/")));
        // The same server spelled another way is the same place.
        assert!(ov(&a, &cfg("http://H:9000/", "b", "/phones/a/")));
    }

    #[test]
    fn a_form_with_nothing_typed_has_the_usual_defaults() {
        // Nothing set up: the usual region, profile and path style, nothing else.
        let f = S3Form::default();
        assert_eq!(
            (f.region.as_str(), f.profile.as_str(), f.path_style),
            ("us-east-1", "default", true)
        );
    }

    #[test]
    fn each_provider_shows_only_its_own_fields_named_as_its_console_names_them() {
        let names = |p| -> Vec<&str> { main_fields(p).iter().map(|f| f.label()).collect() };
        assert_eq!(names(Provider::Aws), ["Region", "Bucket"]);
        assert_eq!(names(Provider::R2), ["Account ID", "Bucket"]);
        assert_eq!(names(Provider::B2), ["Endpoint", "Bucket"]);
        assert_eq!(names(Provider::Wasabi), ["Region", "Bucket"]);
        assert_eq!(names(Provider::Other), ["Server address", "Bucket"]);
        // Region is not asked for where it is not needed; only a server of your own has More.
        for p in [Provider::R2, Provider::B2] {
            assert!(!names(p).contains(&"Region"), "{p:?}");
        }
        for p in [Provider::Aws, Provider::R2, Provider::B2, Provider::Wasabi] {
            assert!(more_fields(p).is_empty(), "{p:?}");
        }
        assert_eq!(
            more_fields(Provider::Other),
            ["Region", "Path-style addressing", "CA file"]
        );
    }

    #[test]
    fn a_providers_fields_become_the_endpoint_region_and_addressing_the_store_uses() {
        let mut f = S3Form {
            provider: Provider::Aws,
            region: "eu-west-1".into(),
            bucket: "bk".into(),
            ..S3Form::default()
        };
        let c = f.config().unwrap();
        assert_eq!(c.endpoint, "https://s3.eu-west-1.amazonaws.com");
        assert!(!c.path_style && c.provider == Provider::Aws && c.account_id.is_none());
        assert_eq!(S3Form::from_config(&c).config().unwrap(), c);

        f.provider = Provider::R2;
        f.account_id = " acc123 ".into();
        let c = f.config().unwrap();
        assert_eq!(c.endpoint, "https://acc123.r2.cloudflarestorage.com");
        assert_eq!(
            (c.region.as_str(), c.account_id.as_deref()),
            ("auto", Some("acc123"))
        );
        assert_eq!(S3Form::from_config(&c).config().unwrap(), c);

        f.provider = Provider::B2;
        f.server = "s3.us-west-004.backblazeb2.com".into();
        let c = f.config().unwrap();
        assert_eq!(c.endpoint, "https://s3.us-west-004.backblazeb2.com");
        assert_eq!(c.region, "us-west-004");
        assert_eq!(S3Form::from_config(&c).config().unwrap(), c);
        f.server = "https://me:pw@s3.us-west-004.backblazeb2.com".into();
        assert!(!f.config().unwrap_err().contains("pw@"));

        f.provider = Provider::Wasabi;
        f.region = "eu-central-1".into();
        let c = f.config().unwrap();
        assert_eq!(c.endpoint, "https://s3.eu-central-1.wasabisys.com");

        // Nothing typed is no server set up; a provider's own field alone is a start.
        assert_eq!(S3Form::default().default_config(), Ok(None));
        let r2 = S3Form {
            provider: Provider::R2,
            account_id: "a".into(),
            ..S3Form::default()
        };
        assert!(r2.default_config().is_err());
    }

    #[test]
    fn the_fields_are_checked_and_turned_into_settings() {
        let mut f = S3Form {
            server: " http://s3.example.test:9000/ ".into(),
            bucket: "pnk-test".into(),
            prefix: "itest/x/".into(),
            profile: "pnk-test".into(),
            credentials_file: "C:/keys/file".into(),
            ..S3Form::default()
        };
        let c = f.config().unwrap();
        assert_eq!(c.endpoint, "http://s3.example.test:9000");
        assert_eq!(c.profile.as_deref(), Some("pnk-test"));
        assert_eq!(c.credentials_file, Some(PathBuf::from("C:/keys/file")));
        assert_eq!(c.ca_file, None);
        assert_eq!(S3Form::from_config(&c).config().unwrap(), c);
        for (what, edit) in [
            (
                "address",
                (|f: &mut S3Form| f.server = "host:9000".into()) as fn(&mut S3Form),
            ),
            ("bucket", |f| f.bucket = String::new()),
            ("bucket", |f| f.bucket = "a/b".into()),
            ("region", |f| f.region = " ".into()),
            ("folder", |f| f.prefix = "a/../b".into()),
            ("user name", |f| {
                f.server = "http://me:pw@s3.example.test:9000".into()
            }),
            ("user", |f| f.server = "https://me@s3.example.test".into()),
            ("query", |f| {
                f.server = "http://s3.example.test:9000/?k=v".into()
            }),
            ("fragment", |f| {
                f.server = "http://s3.example.test:9000#x".into()
            }),
        ] {
            let mut g = f.clone();
            edit(&mut g);
            let e = g.config().unwrap_err();
            assert!(!e.contains("pw@"), "{what}: {e}");
        }
        let e = S3Form {
            server: "http://me:pw@s3.example.test:9000".into(),
            ..f.clone()
        }
        .config()
        .unwrap_err();
        assert!(e.contains("credentials file"), "{e}");
        // An empty profile means "the app's default" rather than a profile named "".
        f.profile = " ".into();
        assert_eq!(f.config().unwrap().profile, None);
        // The default location may be left empty, and then is no default.
        assert_eq!(S3Form::default().default_config(), Ok(None));
        assert!(
            S3Form {
                server: "http://h".into(),
                ..S3Form::default()
            }
            .default_config()
            .is_err()
        );
    }

    #[test]
    fn what_test_and_the_state_check_make_of_each_answer() {
        let offline = io::Error::other(store::Offline("can't reach http://h: down".into()));
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let unsupported = io::Error::new(io::ErrorKind::Unsupported, "no conditional writes");
        let other = io::Error::other("HTTP 500");
        assert_eq!(test_outcome(&Ok(true)), TestOutcome::Ok);
        assert_eq!(test_outcome(&Ok(false)), TestOutcome::BucketNotFound);
        assert!(matches!(
            test_outcome(&Err(offline)),
            TestOutcome::NoNetwork(_)
        ));
        assert_eq!(test_outcome(&Err(denied)), TestOutcome::NoAccess);
        assert!(matches!(
            test_outcome(&Err(unsupported)),
            TestOutcome::ServerUnsupported(_)
        ));
        assert!(matches!(test_outcome(&Err(other)), TestOutcome::Other(_)));
        assert!(matches!(
            open_failure("no S3 credentials for profile \"x\": no credentials file is set"),
            TestOutcome::NoCredentials(_)
        ));
        assert!(matches!(
            open_failure("profile \"x\" in f: the profile is not in the file"),
            TestOutcome::NoCredentials(_)
        ));
        assert!(matches!(
            open_failure("bad endpoint"),
            TestOutcome::Other(_)
        ));
        // No text names a key.
        assert!(
            !TestOutcome::NoAccess
                .text()
                .to_lowercase()
                .contains("secret")
        );
    }

    #[test]
    fn a_buckets_state_comes_from_what_root_exists_said() {
        let state = |r: io::Result<bool>, gone: bool| s3_folder_state(&r, gone).0;
        assert_eq!(state(Ok(true), false), FolderState::Present);
        // Bucket there, nothing of the backup in it, and it had synced: gone.
        assert_eq!(state(Ok(true), true), FolderState::Missing);
        assert_eq!(state(Ok(false), false), FolderState::Missing);
        let offline = io::Error::other(store::Offline("can't reach http://h: down".into()));
        let (s, why) = s3_folder_state(&Err(offline), false);
        assert_eq!(s, FolderState::Offline);
        assert!(why.starts_with("No network: can't reach http://h"), "{why}");
        assert_eq!(
            state(Err(io::Error::from(io::ErrorKind::PermissionDenied)), false),
            FolderState::NoAccess
        );
        assert_eq!(
            state(Err(io::Error::new(io::ErrorKind::Unsupported, "x")), false),
            FolderState::ServerUnsupported
        );
        assert_eq!(
            state(Err(io::Error::other("x")), false),
            FolderState::Unusable
        );
    }

    #[test]
    fn addresses_read_as_s3_urls() {
        assert_eq!(address(&cfg("http://h", "b", "p/q")), "s3://b/p/q/");
        assert_eq!(address(&cfg("http://h", "b", "")), "s3://b/");
    }

    // ---- the data model: old files still load, and folder devices are written as before ----

    use pnk_core::registry::{HistoryHome, KnownDevice, Registry};
    use pnk_core::signals::{DeviceKind, Signals};
    use pnk_core::state::StateDir;

    fn folder_device(dir: &std::path::Path) -> KnownDevice {
        let mut d = KnownDevice::new(
            DeviceKind::Ptp,
            "Old phone",
            &Signals::default(),
            "2026-01-01T00:00:00Z",
        );
        d.backup_dir = Some(dir.to_path_buf());
        d
    }

    fn bucket_device(prefix: &str) -> KnownDevice {
        let mut d = KnownDevice::new(
            DeviceKind::Ptp,
            "Pixel",
            &Signals::default(),
            "2026-01-01T00:00:00Z",
        );
        d.set_destination(Destination::S3(cfg("http://h:9000", "b", prefix)));
        d
    }

    #[test]
    fn a_folder_device_is_written_with_exactly_the_fields_it_always_had() {
        let t = tempfile::tempdir().unwrap();
        let d = folder_device(t.path());
        let v = serde_json::to_value(&d).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "backup_dir",
                "first_seen",
                "history",
                "id",
                "kind",
                "last_seen",
                "last_sync",
                "name",
                "not_this",
                "settings",
                "view"
            ]
        );
        assert!(!serde_json::to_string(&d).unwrap().contains("s3"));
        // And a record or a file from before reads back the same.
        let back: KnownDevice = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
        assert_eq!(
            back.destination(),
            Some(Destination::Local(t.path().to_path_buf()))
        );
    }

    #[test]
    fn a_bucket_device_keeps_its_place_and_no_keys() {
        let mut d = bucket_device("phones/pixel/");
        if let Destination::S3(c) = d.destination().unwrap() {
            let mut c = c;
            c.profile = Some("home".into());
            c.credentials_file = Some(PathBuf::from("C:/keys/aws"));
            d.set_destination(Destination::S3(c));
        }
        let text = serde_json::to_string(&d).unwrap();
        assert!(text.contains("\"s3\"") && text.contains("phones/pixel/"));
        assert!(text.contains("\"home\"") && text.contains("C:/keys/aws"));
        assert!(!text.to_lowercase().contains("secret") && !text.contains("access_key"));
        assert!(d.backup_dir.is_none());
        let back: KnownDevice = serde_json::from_str(&text).unwrap();
        assert_eq!(back, d);
        // Making it a folder device clears the bucket, and the other way round.
        d.set_destination(Destination::Local(PathBuf::from("D:/B")));
        assert!(d.s3.is_none() && d.backup_dir.is_some());
    }

    #[test]
    fn devices_json_from_before_loads_and_a_bucket_device_survives_a_save() {
        let t = tempfile::tempdir().unwrap();
        let state = StateDir::open(t.path().join("state")).unwrap();
        // A whole device with a `backup_dir` and no `s3`, as an earlier version wrote it.
        let mut old = serde_json::to_value(folder_device(&t.path().join("nope"))).unwrap();
        old.as_object_mut().unwrap().remove("s3");
        let file = serde_json::json!({ "version": 2, "devices": [old] });
        std::fs::write(
            state.path().join("devices.json"),
            serde_json::to_vec_pretty(&file).unwrap(),
        )
        .unwrap();
        let reg = Registry::load(&state);
        assert_eq!(reg.devices().len(), 1);
        assert!(matches!(
            reg.devices()[0].destination(),
            Some(Destination::Local(_))
        ));
        // A bucket device saved with the rest comes back whole, and with its place.
        let mut reg = reg;
        let b = bucket_device("phones/pixel/");
        reg.add(b.clone());
        reg.save(&state).unwrap();
        let again = Registry::load(&state);
        let got = again.get(&b.id).expect("the bucket device is listed");
        assert_eq!(got.s3, b.s3);
        assert_eq!(got.backup_dir, None);
        assert!(got.offline.is_none(), "a bucket is never 'away' at load");
        assert_eq!(again.devices().len(), 2);
    }

    #[test]
    fn the_settings_file_from_before_loads_and_the_default_bucket_is_optional() {
        let t = tempfile::tempdir().unwrap();
        let state = StateDir::open(t.path()).unwrap();
        std::fs::write(
            state.path().join("config.json"),
            br#"{"backup_dir": "D:\B"}"#,
        )
        .unwrap();
        let mut c = state.config();
        assert!(c.default_s3.is_none());
        state.save_config(&c).unwrap();
        let text = std::fs::read_to_string(state.path().join("config.json")).unwrap();
        assert!(!text.contains("default_s3"), "{text}");
        c.default_s3 = Some(cfg("http://h", "b", "plug-n-keep/"));
        state.save_config(&c).unwrap();
        assert_eq!(state.config().default_s3, c.default_s3);
    }

    #[test]
    fn history_goes_where_the_devices_record_is() {
        let t = tempfile::tempdir().unwrap();
        let mut reg = Registry::default();
        let b = bucket_device("p/");
        let f = folder_device(t.path());
        let loose = KnownDevice::new(DeviceKind::Ptp, "New", &Signals::default(), "x");
        let (bid, fid, lid) = (b.id.clone(), f.id.clone(), loose.id.clone());
        for d in [b, f, loose] {
            reg.add(d);
        }
        assert_eq!(
            reg.history_home(&bid),
            HistoryHome::Store(Destination::S3(cfg("http://h:9000", "b", "p/")))
        );
        assert_eq!(
            reg.history_home(&fid),
            HistoryHome::Folder(t.path().join(pnk_core::folder_record::HISTORY_FILE))
        );
        assert_eq!(reg.history_home(&lid), HistoryHome::Profile);
        assert_eq!(reg.history_home("unknown"), HistoryHome::Profile);
    }

    #[test]
    fn another_devices_place_overlaps_for_folders_and_buckets() {
        use pnk_core::registry::KnownDevice;
        use pnk_core::signals::{DeviceKind, Signals};
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("a").join("inner"));
        let mut one = KnownDevice::new(DeviceKind::Ptp, "One", &Signals::default(), "now");
        one.set_destination(Destination::Local(a.clone()));
        let mut two = KnownDevice::new(DeviceKind::Ptp, "Two", &Signals::default(), "now");
        two.set_destination(Destination::S3(cfg("http://h:9000", "b", "x/")));
        let known = vec![one.clone(), two.clone()];
        let hit = |own: Option<&str>, d: Destination| {
            overlapping_device(&known, own, &d).map(|k| k.name.clone())
        };
        assert_eq!(hit(None, Destination::Local(b)).as_deref(), Some("One"));
        assert_eq!(hit(Some(&one.id), Destination::Local(a)), None);
        assert_eq!(hit(None, Destination::Local(t.path().join("c"))), None);
        let inside = Destination::S3(cfg("http://h:9000", "b", "x/y/"));
        assert_eq!(hit(None, inside.clone()).as_deref(), Some("Two"));
        assert_eq!(hit(Some(&two.id), inside), None);
    }
}
