//! Where a device's backup is kept: a folder on this PC or a place in a bucket. Holds no keys;
//! the credentials come from the profile the S3 settings name.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{BackupStore, LocalStore, S3Config, S3Store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Destination {
    Local(PathBuf),
    S3(S3Config),
}

impl Destination {
    /// The store for this destination. For S3 this reads the credentials but sends nothing.
    pub fn open(&self) -> Result<Box<dyn BackupStore>, String> {
        match self {
            Destination::Local(path) => Ok(Box::new(LocalStore::new(path))),
            Destination::S3(cfg) => S3Store::new(cfg.clone()).map(|s| Box::new(s) as _),
        }
    }

    /// Whether `other` names the same backup. Folders are the same when they resolve to the same
    /// place (a folder that can't be resolved is never the same as anything). Buckets are the same
    /// by endpoint, bucket and prefix, spelled in any of the equivalent ways.
    pub fn same_place(&self, other: &Destination) -> bool {
        match (self, other) {
            (Destination::Local(a), Destination::Local(b)) => same_folder(a, b),
            (Destination::S3(a), Destination::S3(b)) => {
                normalise_endpoint(&a.endpoint) == normalise_endpoint(&b.endpoint)
                    && a.bucket == b.bucket
                    && super::s3::normalise_prefix(&a.prefix)
                        == super::s3::normalise_prefix(&b.prefix)
            }
            _ => false,
        }
    }

    /// For messages, and as the path-shaped name a plan and a crash record carry for a backup
    /// that is not a folder: the folder, or `s3://bucket/prefix`.
    pub fn label(&self) -> PathBuf {
        match self {
            Destination::Local(p) => p.clone(),
            Destination::S3(c) => PathBuf::from(format!(
                "s3://{}/{}",
                c.bucket,
                super::s3::normalise_prefix(&c.prefix)
            )),
        }
    }
}

/// The name of the folder `dev` is, if it is directly inside the base folder `base` (same server
/// and bucket, and `<base>/<name>/` with one name); else `None`. In S3 mode every device's
/// folder is like this, so any PC that has the connection finds it.
pub fn directly_under(base: &S3Config, dev: &S3Config) -> Option<String> {
    if normalise_endpoint(&base.endpoint) != normalise_endpoint(&dev.endpoint)
        || base.bucket != dev.bucket
    {
        return None;
    }
    let (b, d) = (
        super::s3::normalise_prefix(&base.prefix),
        super::s3::normalise_prefix(&dev.prefix),
    );
    let rest = d.strip_prefix(&b)?.trim_end_matches('/');
    (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_string())
}

fn same_folder(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `scheme://host:port` in lower case with the default port made explicit and no trailing `/`.
fn normalise_endpoint(e: &str) -> String {
    let e = e.trim();
    match url::Url::parse(e) {
        Ok(u) if u.host_str().is_some() => format!(
            "{}://{}:{}{}",
            u.scheme(),
            u.host_str().unwrap_or_default().to_ascii_lowercase(),
            u.port_or_known_default().unwrap_or(0),
            u.path().trim_end_matches('/')
        ),
        _ => e.trim_end_matches('/').to_ascii_lowercase(),
    }
}

/// Whether one of the two places contains the other, or they are the same: two backups
/// that overlap would each be read as part of the other, so setup refuses it. Buckets
/// overlap when endpoint and bucket match and one normalised prefix is the other's start on
/// a `/` boundary (an empty prefix is the whole bucket). Folders overlap when one resolves
/// inside the other (a folder not made yet is resolved through its nearest parent). A folder
/// and a bucket never overlap.
pub fn overlaps(a: &Destination, b: &Destination) -> bool {
    match (a, b) {
        (Destination::Local(x), Destination::Local(y)) => {
            let (x, y) = (crate::plan::comparable(x), crate::plan::comparable(y));
            x.starts_with(&y) || y.starts_with(&x)
        }
        (Destination::S3(x), Destination::S3(y)) => {
            if normalise_endpoint(&x.endpoint) != normalise_endpoint(&y.endpoint)
                || x.bucket != y.bucket
            {
                return false;
            }
            let (px, py) = (
                super::s3::normalise_prefix(&x.prefix),
                super::s3::normalise_prefix(&y.prefix),
            );
            // Both end in `/` (or are empty), so a start is a start on a `/` boundary.
            px.starts_with(&py) || py.starts_with(&px)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3(endpoint: &str, bucket: &str, prefix: &str) -> Destination {
        Destination::S3(S3Config {
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
        })
    }

    #[test]
    fn buckets_match_by_endpoint_bucket_and_prefix_however_spelled() {
        let a = s3("http://Host:9000/", "b", "phones/a");
        assert!(a.same_place(&s3("http://host:9000", "b", "/phones/a/")));
        assert!(s3("https://h", "b", "").same_place(&s3("https://H:443/", "b", "/")));
        assert!(!a.same_place(&s3("http://host:9000", "b", "phones/b")));
        assert!(!a.same_place(&s3("http://host:9000", "c", "phones/a")));
        assert!(!a.same_place(&s3("http://host:9001", "b", "phones/a")));
        // Prefixes that only share a start are different places.
        assert!(!s3("http://h", "b", "p").same_place(&s3("http://h", "b", "pp")));
    }

    #[test]
    fn buckets_overlap_when_one_prefix_holds_the_other() {
        let o = |a: &Destination, b: &Destination| overlaps(a, b) && overlaps(b, a);
        let a = s3("http://Host:9000/", "b", "phones/a");
        assert!(o(&a, &a));
        assert!(o(&a, &s3("http://host:9000", "b", "/phones/a/")));
        assert!(o(&a, &s3("http://host:9000", "b", "phones/a/inner")));
        assert!(o(&a, &s3("http://host:9000", "b", "phones")));
        // The whole bucket holds everything in it.
        assert!(o(&a, &s3("http://host:9000", "b", "")));
        assert!(o(&a, &s3("http://host:9000", "b", "/")));
        // Only a shared start is not nesting; other bucket or server never is.
        assert!(!overlaps(&a, &s3("http://host:9000", "b", "phones/ab")));
        assert!(!overlaps(
            &s3("http://h", "b", "p"),
            &s3("http://h", "b", "pp")
        ));
        assert!(!overlaps(&a, &s3("http://host:9000", "c", "phones/a")));
        assert!(!overlaps(&a, &s3("http://other:9000", "b", "phones/a")));
        assert!(!overlaps(
            &a,
            &Destination::Local(PathBuf::from("phones/a"))
        ));
    }

    #[test]
    fn a_device_folder_is_directly_under_the_base_or_it_is_not() {
        let base = |p: &str| match s3("http://Host:9000/", "b", p) {
            Destination::S3(c) => c,
            Destination::Local(_) => unreachable!(),
        };
        let under = |b: &str, d: &str, endpoint: &str, bucket: &str| match s3(endpoint, bucket, d) {
            Destination::S3(c) => directly_under(&base(b), &c),
            Destination::Local(_) => unreachable!(),
        };
        let here = "http://host:9000";
        assert_eq!(
            under("devices", "devices/Pixel 8/", here, "b").as_deref(),
            Some("Pixel 8")
        );
        assert_eq!(
            under("/devices/", "devices/Pixel", here, "b").as_deref(),
            Some("Pixel")
        );
        // An empty base is the bucket's top.
        assert_eq!(under("", "Pixel/", here, "b").as_deref(), Some("Pixel"));
        // The base itself, a folder deeper, a neighbour of the base, another bucket or server.
        assert_eq!(under("devices", "devices/", here, "b"), None);
        assert_eq!(under("devices", "devices/a/b/", here, "b"), None);
        assert_eq!(under("devices", "devices2/a/", here, "b"), None);
        assert_eq!(under("devices", "other/a/", here, "b"), None);
        assert_eq!(under("devices", "devices/a/", here, "c"), None);
        assert_eq!(
            under("devices", "devices/a/", "http://other:9000", "b"),
            None
        );
        assert_eq!(under("", "", here, "b"), None);
    }

    #[test]
    fn folders_overlap_by_containment() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("ab"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let l = |p: &Path| Destination::Local(p.to_path_buf());
        assert!(overlaps(&l(&a), &l(&a)));
        assert!(overlaps(&l(t.path()), &l(&a)) && overlaps(&l(&a), &l(t.path())));
        // A folder not made yet is judged by where it would be.
        assert!(overlaps(&l(&a), &l(&a.join("new").join("deeper"))));
        assert!(!overlaps(&l(&a), &l(&b)));
        assert!(!overlaps(&l(&a), &l(&t.path().join("a2").join("x"))));
    }

    #[test]
    fn folders_match_when_they_resolve_to_the_same_place() {
        let t = tempfile::tempdir().unwrap();
        let sub = t.path().join("x");
        std::fs::create_dir(&sub).unwrap();
        let a = Destination::Local(sub.clone());
        assert!(a.same_place(&Destination::Local(sub.join("..").join("x"))));
        assert!(!a.same_place(&Destination::Local(t.path().to_path_buf())));
        // A folder that is not there is the same as nothing, itself included.
        let gone = Destination::Local(t.path().join("nope"));
        assert!(!gone.same_place(&gone));
        assert!(!a.same_place(&s3("http://h", "b", "")));
    }

    #[test]
    fn a_destination_round_trips_and_labels_itself() {
        let d = s3("http://h:1", "b", "p/q");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<Destination>(&json).unwrap(), d);
        assert_eq!(d.label(), PathBuf::from("s3://b/p/q/"));
        let l = Destination::Local(PathBuf::from("x"));
        assert_eq!(
            serde_json::from_str::<Destination>(&serde_json::to_string(&l).unwrap()).unwrap(),
            l
        );
    }
}
