//! A bucket on an S3-compatible server as a backup store. Keys are `prefix + rel`, `/`-separated.
//!
//! Needs a server with conditional writes (`If-None-Match: *`, `If-Match`); the first
//! [`root_exists`](BackupStore::root_exists) checks that the server really honours them. Uploads
//! are buffered in memory: one part (16 MiB) at a time, sent in turn, no concurrency yet.
//! Modified times travel as the `x-amz-meta-pnk-mtime` header (ms since the epoch); the
//! listing does not carry them yet (phase 5: a cache object of key to mtime).
//!
//! Every write carries a SHA-256 the server checks (`x-amz-checksum-sha256`) and a random
//! `x-amz-meta-pnk-token`. A request that was sent again after a lost answer may find its own
//! earlier try already done; the token is how that is told from somebody else's file.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::warn;
use rusty_s3::actions::{
    CompleteMultipartUpload, CreateMultipartUpload, ListObjectsV2, ListObjectsV2Response, ListParts,
};
use rusty_s3::{Method, S3Action};
use serde::{Deserialize, Serialize};

use super::{BackupStore, Destination, Kind, ListIssue, Listed, Listing, Stat, Tag, Upload};
use crate::paths::SafeRelPath;
use crate::platform::DiskSpace;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod fake_tests;
mod http;
pub mod provider;
mod sum;
mod upload;
mod xml;

#[cfg(test)]
pub(crate) mod tests;

use http::Client;
pub use provider::Provider;

/// Non-final parts must be at least 5 MiB for S3.
const PART_SIZE: usize = 16 * 1024 * 1024;
/// A single CopyObject takes at most 5 GiB.
const COPY_LIMIT: u64 = 5 * 1024 * 1024 * 1024;
const COPY_PART: u64 = 512 * 1024 * 1024;

pub(super) const MPU_REF: &str = "s3mpu:";
pub(super) const PUT_REF: &str = "s3put:";
/// The start of the name of the object the server self-test writes, under the backup's prefix.
pub const PROBE_PREFIX: &str = ".plug-n-keep-probe-";
/// What a refused self-test says.
const NO_CONDITIONAL_WRITES: &str = "this S3 server does not support conditional writes, which Plug-n-Keep needs to never overwrite a file";
/// Notes a Test can show when the server works but is not perfect.
const NO_CHECKSUMS: &str = "This server doesn't check upload checksums.";
const CHECKSUMS_NOT_CHECKED: &str = "This server doesn't verify upload checksums.";
const H_TOKEN: &str = "x-amz-meta-pnk-token";
const H_SUM: &str = "x-amz-checksum-sha256";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Config {
    /// Which service the bucket is on. The fields below are what its own settings come to
    /// ([`Provider::derive`]); the store only looks at those.
    #[serde(default, skip_serializing_if = "Provider::is_other")]
    pub provider: Provider,
    /// Cloudflare R2's Account ID, which the endpoint is made from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// `http://host:port` or `https://host`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// The backup's folder inside the bucket; may be empty.
    #[serde(default)]
    pub prefix: String,
    /// Which profile of the credentials file to use (else `AWS_PROFILE`, else `default`).
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub credentials_file: Option<PathBuf>,
    #[serde(default)]
    pub path_style: bool,
    /// A PEM file with the CA that signed the server's certificate. When set, only it is trusted.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
}

/// Sizes that tests lower so the big-file paths run on small data.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub part_size: usize,
    pub copy_limit: u64,
    pub copy_part: u64,
    /// Keys asked for per listing page.
    pub list_page: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            part_size: PART_SIZE,
            copy_limit: COPY_LIMIT,
            copy_part: COPY_PART,
            list_page: 1000,
        }
    }
}

pub(super) struct Inner {
    client: Client,
    prefix: String,
    limits: Limits,
    /// What the server self-test found, once it has found something for certain.
    probed: std::sync::Mutex<Option<bool>>,
    /// Whether uploads carry `x-amz-checksum-sha256`. Off when the server's self-test found
    /// it does not take the header.
    sums: std::sync::atomic::AtomicBool,
    /// Things the self-test found that are worth saying (not failures).
    notes: std::sync::Mutex<Vec<String>>,
}

#[derive(Clone)]
pub struct S3Store {
    inner: Arc<Inner>,
    config: S3Config,
}

/// What a HeadObject says.
#[derive(Debug, Clone)]
pub(super) struct Head {
    pub size: u64,
    pub etag: Option<Tag>,
    pub mtime_ms: Option<String>,
    /// The write token ([`sum::new_token`]) the object was made with, if it has one.
    pub token: Option<String>,
    /// The SHA-256 the server holds for it (base64), if it holds one.
    pub checksum: Option<String>,
}

impl Head {
    /// Whether the two objects can be the same bytes by their checksums: true when either has
    /// none, or they are not comparable (a multipart object's is of its parts), or they agree.
    pub(super) fn checksums_agree(&self, other: &Head) -> bool {
        match (&self.checksum, &other.checksum) {
            (Some(a), Some(b)) if !a.contains('-') && !b.contains('-') => a == b,
            _ => true,
        }
    }
}

/// Whether a server address can be used: no `user:pass@` (any `@` before the first `/`), no
/// `?query` and no `#fragment`. Keys belong in the credentials file, so the message never
/// repeats the address (it could hold a secret).
pub(crate) fn check_endpoint(endpoint: &str) -> Result<(), String> {
    let e = endpoint.trim();
    let after_scheme = e.split_once("://").map_or(e, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') || e.contains('?') || e.contains('#') {
        return Err(
            "the S3 server's address can't have a user name, password, ? or # in it: put keys in the credentials file, not in the server address"
                .into(),
        );
    }
    Ok(())
}

/// Whether a prefix can be used: no `.` or `..` or empty folder names (apart from slashes at
/// either end), no backslashes and no control characters.
pub(crate) fn check_prefix(p: &str) -> Result<(), String> {
    let t = p.trim().trim_matches('/');
    if t.is_empty() {
        return Ok(());
    }
    if t.contains('\\') || t.chars().any(char::is_control) {
        return Err(format!(
            "the S3 folder \"{p}\" can't be used: no backslashes or control characters"
        ));
    }
    if t.split('/').any(|seg| matches!(seg, "" | "." | "..")) {
        return Err(format!(
            "the S3 folder \"{p}\" can't be used: no empty, \".\" or \"..\" parts"
        ));
    }
    Ok(())
}

/// `no leading '/'`, exactly one trailing `/` unless empty.
pub(crate) fn normalise_prefix(p: &str) -> String {
    let t = p.trim().trim_matches('/');
    if t.is_empty() {
        String::new()
    } else {
        format!("{t}/")
    }
}

/// ETags come quoted (and sometimes weak) in headers and listings; keep the bare value.
pub(super) fn normalise_etag(t: &str) -> Tag {
    let t = t.trim();
    let t = t.strip_prefix("W/").unwrap_or(t);
    t.trim_matches('"').to_string()
}

fn quoted(tag: &str) -> String {
    format!("\"{tag}\"")
}

pub(super) fn mtime_ms(t: SystemTime) -> Option<String> {
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some(d.as_millis().to_string())
}

fn parse_mtime(ms: &str) -> Option<SystemTime> {
    ms.trim()
        .parse::<u64>()
        .ok()
        .map(|m| UNIX_EPOCH + Duration::from_millis(m))
}

pub(super) fn already_exists(key: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{key} already exists"),
    )
}

/// A key as `x-amz-copy-source` wants it: `/bucket/key`, percent-encoded except `/`.
fn copy_source(bucket: &str, key: &str) -> String {
    let mut out = String::from("/");
    for b in format!("{bucket}/{key}").bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The `(first, last)` byte of each piece of `size` bytes cut into `piece`s.
pub(super) fn ranges(size: u64, piece: u64) -> Vec<(u64, u64)> {
    let mut v = Vec::new();
    let mut at = 0;
    while at < size {
        let end = (at + piece).min(size) - 1;
        v.push((at, end));
        at = end + 1;
    }
    v
}

impl S3Store {
    pub fn new(config: S3Config) -> Result<Self, String> {
        Self::with_limits(config, Limits::default())
    }

    pub(crate) fn with_limits(config: S3Config, limits: Limits) -> Result<Self, String> {
        check_endpoint(&config.endpoint)?;
        check_prefix(&config.prefix)?;
        let creds = super::creds::load(
            config.profile.as_deref(),
            config.credentials_file.as_deref(),
        )?;
        let client = Client::new(
            &config.endpoint,
            &config.region,
            &config.bucket,
            config.path_style,
            &creds,
            config.ca_file.as_deref(),
        )?;
        Ok(S3Store {
            inner: Arc::new(Inner {
                client,
                prefix: normalise_prefix(&config.prefix),
                limits,
                probed: std::sync::Mutex::new(None),
                sums: std::sync::atomic::AtomicBool::new(true),
                notes: std::sync::Mutex::new(Vec::new()),
            }),
            config,
        })
    }

    /// A store on a pretend server (tests), under the prefix `p/` of the bucket `b`.
    #[cfg(test)]
    pub(crate) fn on_fake(limits: Limits) -> (S3Store, fake::FakeS3) {
        let (_creds, cfg) = tests::offline_config();
        let mut s = S3Store::with_limits(cfg, limits)
            .expect("test settings")
            .fast_retries();
        let fake = fake::FakeS3::new();
        Arc::get_mut(&mut s.inner)
            .expect("not shared yet")
            .client
            .fake = Some(fake.transport());
        (s, fake)
    }

    /// A store for `cfg` on an existing pretend server, so two places can share one bucket (tests).
    #[cfg(test)]
    pub(crate) fn on_fake_with(cfg: S3Config, fake: &fake::FakeS3) -> S3Store {
        let mut s = S3Store::new(cfg).expect("test settings").fast_retries();
        Arc::get_mut(&mut s.inner)
            .expect("not shared yet")
            .client
            .fake = Some(fake.transport());
        s
    }

    /// Shortens the wait between tries (tests).
    #[cfg(test)]
    pub(crate) fn fast_retries(mut self) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("not shared yet");
        inner.client.backoff = Duration::from_millis(5);
        self
    }

    pub fn prefix(&self) -> &str {
        &self.inner.prefix
    }

    pub fn config(&self) -> &S3Config {
        &self.config
    }

    /// Ids (`upload id` and key) of the unfinished multipart uploads under the prefix.
    #[cfg(test)]
    pub(crate) fn open_upload_refs(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let mut marker: Option<(String, String)> = None;
        loop {
            let found = self.inner.list_uploads(marker.as_ref())?;
            for (key, id, _) in &found.items {
                if self.inner.own_key(key).is_some() {
                    out.push(format!("{MPU_REF}{id}:{key}"));
                }
            }
            match found.next {
                Some(m) => marker = Some(m),
                None => return Ok(out),
            }
        }
    }

    /// Every key under the prefix, as the server lists it: nothing skipped, so also the folder
    /// placeholders and the names `list` refuses (tests).
    #[cfg(any(test, feature = "test-support"))]
    fn raw_keys(&self) -> io::Result<Vec<String>> {
        let inner = &self.inner;
        let c = &inner.client;
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let r = c.exec(Method::Get, &[], &[], |h| {
                let mut a = ListObjectsV2::new(&c.bucket, Some(&c.creds));
                a.with_prefix(inner.prefix.clone());
                if let Some(t) = &token {
                    a.with_continuation_token(t.clone());
                }
                Client::presign(a, h)
            })?;
            if !r.ok() {
                return Err(c.fail(&r, "list the bucket"));
            }
            let page = ListObjectsV2::parse_response(&r.text())
                .map_err(|_| io::Error::other("unreadable listing"))?;
            keys.extend(page.contents.into_iter().map(|o| o.key));
            match page.next_continuation_token {
                Some(t) => token = Some(t),
                None => return Ok(keys),
            }
        }
    }

    /// Test cleanup: aborts the unfinished uploads and deletes every object under the prefix
    /// (whatever it is called). Only built for tests (and for the app's tests, through the
    /// `test-support` feature), and it does nothing unless `PNK_S3_TEST=1` is set and the store's
    /// prefix is under `itest/` in the bucket `pnk-test`.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn purge_test_prefix(&self) {
        let inner = &self.inner;
        let own = std::env::var("PNK_S3_TEST").ok().as_deref() == Some("1")
            && inner.prefix.starts_with("itest/")
            && inner.prefix.len() > "itest/".len()
            && inner.client.bucket.name() == "pnk-test";
        if !own {
            return;
        }
        let _ = self.clean_stale(Duration::ZERO);
        if let Ok(keys) = self.raw_keys() {
            for key in keys {
                assert!(key.starts_with("itest/") && key.starts_with(&inner.prefix));
                let _ = inner.delete(&key, None);
            }
        }
    }

    /// Aborts the unfinished multipart uploads under the prefix that were started more than
    /// `older_than` ago. Returns how many.
    fn clean_stale(&self, older_than: Duration) -> io::Result<usize> {
        let inner = &self.inner;
        let cutoff = SystemTime::now()
            .checked_sub(older_than)
            .unwrap_or(UNIX_EPOCH);
        let mut done = 0;
        let mut marker: Option<(String, String)> = None;
        loop {
            let found = inner.list_uploads(marker.as_ref())?;
            for (key, id, started) in &found.items {
                if inner.own_key(key).is_some() && *started < cutoff {
                    inner.abort_multipart(key, id)?;
                    done += 1;
                }
            }
            match found.next {
                Some(m) => marker = Some(m),
                None => return Ok(done),
            }
        }
    }
}

impl Inner {
    fn key(&self, rel: &SafeRelPath) -> String {
        format!("{}{}", self.prefix, rel.as_str())
    }

    /// `key` as a path under this store's prefix, when it is one a backup could hold (it is
    /// under the prefix and the rest is a plain relative path). The checks that make a crash
    /// record or a listing safe to act on.
    pub(super) fn own_key<'k>(&self, key: &'k str) -> Option<&'k str> {
        let rest = key.strip_prefix(self.prefix.as_str())?;
        SafeRelPath::parse(rest).ok().map(|_| rest)
    }

    pub(super) fn head(&self, key: &str) -> io::Result<Option<Head>> {
        let c = &self.client;
        let hdrs = [("x-amz-checksum-mode", "ENABLED")];
        let r = c.exec(Method::Head, &hdrs, &[], |h| {
            Client::presign(c.bucket.head_object(Some(&c.creds), key), h)
        })?;
        match r.status {
            404 => Ok(None),
            _ if r.ok() => {
                // A missing size must not read as 0: two 0s would "verify" a move.
                let size = r
                    .header("content-length")
                    .and_then(|v| v.trim().parse().ok())
                    .ok_or_else(|| io::Error::other(format!("S3 gave no size for {key}")))?;
                Ok(Some(Head {
                    size,
                    etag: r.header("etag").map(normalise_etag),
                    mtime_ms: r.header("x-amz-meta-pnk-mtime").map(str::to_string),
                    token: r.header(H_TOKEN).map(str::to_string),
                    checksum: r.header(H_SUM).map(str::to_string),
                }))
            }
            _ => Err(c.fail(&r, &format!("look at {key}"))),
        }
    }

    /// The tag of `key` if it is the object an earlier try of ours wrote: it has our `token`
    /// and `size`. Only asked after a request that was sent again (see [`http::Resp::retried`]).
    /// Anything unclear is "no".
    fn own_write(&self, key: &str, token: &str, size: u64) -> Option<Tag> {
        let h = self.head(key).ok().flatten()?;
        (h.token.as_deref() == Some(token) && h.size == size)
            .then_some(h.etag)
            .flatten()
    }

    pub(super) fn create_multipart(
        &self,
        key: &str,
        mtime_ms: Option<&str>,
        token: Option<&str>,
    ) -> io::Result<String> {
        let c = &self.client;
        let mut hdrs: Vec<(&str, &str)> = Vec::new();
        if let Some(m) = mtime_ms {
            hdrs.push(("x-amz-meta-pnk-mtime", m));
        }
        if let Some(t) = token {
            hdrs.push((H_TOKEN, t));
        }
        let r = c.exec(Method::Post, &hdrs, &[], |h| {
            Client::presign(
                CreateMultipartUpload::new(&c.bucket, Some(&c.creds), key),
                h,
            )
        })?;
        if !r.ok() {
            return Err(c.fail(&r, &format!("start the upload of {key}")));
        }
        let parsed = CreateMultipartUpload::parse_response(&r.text())
            .map_err(|_| io::Error::other(format!("S3 gave no upload id for {key}")))?;
        let id = parsed.upload_id().to_string();
        if id.is_empty() || id.contains(':') {
            return Err(io::Error::other(format!(
                "S3 gave an unusable upload id for {key}"
            )));
        }
        Ok(id)
    }

    pub(super) fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u16,
        data: &[u8],
    ) -> io::Result<String> {
        let c = &self.client;
        let sum = self.sum_header(data);
        let hdrs: Vec<(&str, &str)> = sum.iter().map(|s| (H_SUM, s.as_str())).collect();
        let r = c.exec(Method::Put, &hdrs, data, |h| {
            Client::presign(c.bucket.upload_part(Some(&c.creds), key, number, id), h)
        })?;
        if !r.ok() {
            return Err(c.fail(&r, &format!("send part {number} of {key}")));
        }
        r.header("etag")
            .map(str::to_string)
            .ok_or_else(|| io::Error::other(format!("S3 gave no ETag for part {number} of {key}")))
    }

    /// An upload that is already gone is fine.
    pub(super) fn abort_multipart(&self, key: &str, id: &str) -> io::Result<()> {
        let c = &self.client;
        let r = c.exec(Method::Delete, &[], &[], |h| {
            Client::presign(c.bucket.abort_multipart_upload(Some(&c.creds), key, id), h)
        })?;
        if r.ok() || r.status == 404 {
            Ok(())
        } else {
            Err(c.fail(&r, &format!("cancel the upload of {key}")))
        }
    }

    fn list_uploads(&self, marker: Option<&(String, String)>) -> io::Result<xml::Uploads> {
        let c = &self.client;
        let r = c.exec(Method::Get, &[], &[], |h| {
            let mut a = ListObjectsV2::new(&c.bucket, Some(&c.creds));
            let q = a.query_mut();
            q.remove("list-type");
            q.remove("encoding-type");
            q.insert("uploads", "");
            if !self.prefix.is_empty() {
                q.insert("prefix", self.prefix.clone());
            }
            if let Some((k, u)) = marker {
                if !k.is_empty() {
                    q.insert("key-marker", k.clone());
                }
                if !u.is_empty() {
                    q.insert("upload-id-marker", u.clone());
                }
            }
            Client::presign(a, h)
        })?;
        if !r.ok() {
            return Err(c.fail(&r, "list unfinished uploads"));
        }
        Ok(xml::parse_uploads(&r.text()))
    }

    fn delete(&self, key: &str, if_match: Option<&Tag>) -> io::Result<()> {
        let c = &self.client;
        let q = if_match.map(|t| quoted(t));
        let mut hdrs: Vec<(&str, &str)> = Vec::new();
        if let Some(q) = &q {
            hdrs.push(("if-match", q));
        }
        let r = c.exec(Method::Delete, &hdrs, &[], |h| {
            Client::presign(c.bucket.delete_object(Some(&c.creds), key), h)
        })?;
        if r.ok() || r.status == 404 {
            Ok(())
        } else {
            Err(c.fail(&r, &format!("delete {key}")))
        }
    }

    /// Copies `from` to `to` on the server, refusing if `to` exists (`AlreadyExists`) or if
    /// `from` no longer has `src.etag`. Keeps the modified-time header.
    pub(super) fn copy(&self, from: &str, to: &str, src: &Head) -> io::Result<()> {
        if src.size > self.limits.copy_limit {
            self.copy_multipart(from, to, src)
        } else {
            self.copy_single(from, to, src)
        }
    }

    fn copy_headers(&self, from: &str, src: &Head) -> Vec<(&'static str, String)> {
        let mut v = vec![(
            "x-amz-copy-source",
            copy_source(self.client.bucket.name(), from),
        )];
        if let Some(t) = &src.etag {
            v.push(("x-amz-copy-source-if-match", quoted(t)));
        }
        v
    }

    /// A 412 on a copy is either the destination being taken or the source having changed.
    fn copy_refused(&self, from: &str, to: &str) -> io::Error {
        match self.head(to) {
            Ok(Some(_)) => already_exists(to),
            _ => io::Error::other(format!(
                "{from} changed while it was being copied, so nothing was moved"
            )),
        }
    }

    /// After a copy request that was sent again got "already there": whether `to` is the copy
    /// an earlier try of ours made. It is when it matches `src` in size, modified time and
    /// checksum (and write token, if `src` has one) and `src` is unchanged. A source with
    /// nothing to tell it by never counts.
    fn copy_is_done(&self, from: &str, to: &str, src: &Head) -> bool {
        if src.mtime_ms.is_none() && src.token.is_none() && src.checksum.is_none() {
            return false;
        }
        let (Ok(Some(dest)), Ok(Some(now))) = (self.head(to), self.head(from)) else {
            return false;
        };
        dest.size == src.size
            && dest.mtime_ms == src.mtime_ms
            && dest.checksums_agree(src)
            && (src.token.is_none() || dest.token == src.token)
            && now.etag == src.etag
    }

    fn copy_single(&self, from: &str, to: &str, src: &Head) -> io::Result<()> {
        let c = &self.client;
        let owned = self.copy_headers(from, src);
        let mut hdrs: Vec<(&str, &str)> = vec![("if-none-match", "*")];
        hdrs.extend(owned.iter().map(|(k, v)| (*k, v.as_str())));
        hdrs.push(("x-amz-metadata-directive", "COPY"));
        hdrs.sort_by_key(|(k, _)| *k);
        let r = c.exec(Method::Put, &hdrs, &[], |h| {
            Client::presign(c.bucket.put_object(Some(&c.creds), to), h)
        })?;
        let text = r.text();
        if r.status == 412
            || (r.ok() && xml::error_code(&text).as_deref() == Some("PreconditionFailed"))
        {
            if r.retried && self.copy_is_done(from, to, src) {
                return Ok(());
            }
            return Err(self.copy_refused(from, to));
        }
        if r.status == 404 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{from} is not there"),
            ));
        }
        if !r.ok() || xml::is_error_body(&text) {
            return Err(c.fail(&r, &format!("copy {from} to {to}")));
        }
        Ok(())
    }

    fn copy_multipart(&self, from: &str, to: &str, src: &Head) -> io::Result<()> {
        let c = &self.client;
        let id = self.create_multipart(to, src.mtime_ms.as_deref(), None)?;
        let sent = (|| -> io::Result<Vec<String>> {
            let owned = self.copy_headers(from, src);
            let mut etags = Vec::new();
            for (n, (first, last)) in ranges(src.size, self.limits.copy_part)
                .into_iter()
                .enumerate()
            {
                let number = u16::try_from(n + 1)
                    .map_err(|_| io::Error::other(format!("{from} is too big to copy")))?;
                let range = format!("bytes={first}-{last}");
                let mut hdrs: Vec<(&str, &str)> =
                    owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
                hdrs.push(("x-amz-copy-source-range", &range));
                hdrs.sort_by_key(|(k, _)| *k);
                let r = c.exec(Method::Put, &hdrs, &[], |h| {
                    Client::presign(c.bucket.upload_part(Some(&c.creds), to, number, &id), h)
                })?;
                let text = r.text();
                if r.status == 412 {
                    return Err(io::Error::other(format!(
                        "{from} changed while it was being copied, so nothing was moved"
                    )));
                }
                if !r.ok() || xml::is_error_body(&text) {
                    return Err(c.fail(&r, &format!("copy part {number} of {from}")));
                }
                let etag = xml::tag(&text, "ETag").map(xml::unescape).ok_or_else(|| {
                    io::Error::other(format!("S3 gave no ETag for a part of {to}"))
                })?;
                etags.push(etag);
            }
            Ok(etags)
        })();
        let etags = match sent {
            Ok(e) => e,
            Err(e) => {
                let _ = self.abort_multipart(to, &id);
                return Err(e);
            }
        };
        let same = |d: &Head| {
            d.size == src.size
                && d.mtime_ms == src.mtime_ms
                && d.checksums_agree(src)
                && (src.mtime_ms.is_some() || src.token.is_some())
                && self
                    .head(from)
                    .ok()
                    .flatten()
                    .is_some_and(|now| now.etag == src.etag)
        };
        match self.complete(to, &id, &etags, Some(&same)) {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = self.abort_multipart(to, &id);
                Err(e)
            }
        }
    }

    /// Finishes a multipart upload with `If-None-Match: *`: `AlreadyExists` if the name is
    /// taken (the upload is then gone); any other failure leaves it for `abort_multipart`.
    /// After a request that was sent again that gets "taken" or "no such upload", `same` says
    /// whether the object now there is the result of this very upload (an earlier try went
    /// through); without a `same` that is never assumed.
    pub(super) fn complete(
        &self,
        key: &str,
        id: &str,
        etags: &[String],
        same: Option<&dyn Fn(&Head) -> bool>,
    ) -> io::Result<Tag> {
        let c = &self.client;
        let parts = || etags.iter().map(String::as_str);
        let body = CompleteMultipartUpload::new(&c.bucket, Some(&c.creds), key, id, parts()).body();
        let hdrs = [("if-none-match", "*")];
        let r = c.exec(Method::Post, &hdrs, body.as_bytes(), |h| {
            Client::presign(
                CompleteMultipartUpload::new(&c.bucket, Some(&c.creds), key, id, parts()),
                h,
            )
        })?;
        let text = r.text();
        let code = xml::error_code(&text);
        let refused = r.status == 412
            || (r.ok()
                && xml::is_error_body(&text)
                && code.as_deref() == Some("PreconditionFailed"));
        let no_upload = r.status == 404 && code.as_deref() != Some("NoSuchBucket");
        if (refused || no_upload)
            && r.retried
            && let Some(same) = same
            && let Ok(Some(h)) = self.head(key)
            && same(&h)
            && let Some(tag) = h.etag
        {
            return Ok(tag);
        }
        if refused {
            let _ = self.abort_multipart(key, id);
            return Err(already_exists(key));
        }
        if !r.ok() || xml::is_error_body(&text) {
            return Err(c.fail(&r, &format!("finish {key}")));
        }
        match rusty_s3::actions::CompleteMultipartUploadResponse::parse(&text) {
            Ok(p) => Ok(normalise_etag(p.etag())),
            Err(_) => self
                .head(key)?
                .and_then(|h| h.etag)
                .ok_or_else(|| io::Error::other(format!("{key} vanished after its write"))),
        }
    }

    /// The parts of an unfinished upload as the server holds them: number, ETag, size.
    pub(super) fn list_parts(&self, key: &str, id: &str) -> io::Result<Vec<(u16, String, u64)>> {
        let c = &self.client;
        let mut out = Vec::new();
        let mut marker: Option<u16> = None;
        loop {
            let r = c.exec(Method::Get, &[], &[], |h| {
                let mut a = c.bucket.list_parts(Some(&c.creds), key, id);
                if let Some(m) = marker {
                    a.set_part_number_marker(m);
                }
                Client::presign(a, h)
            })?;
            if !r.ok() {
                return Err(c.fail(&r, &format!("list the parts of the upload of {key}")));
            }
            let page = ListParts::parse_response(&r.text())
                .map_err(|_| io::Error::other(format!("unreadable part list for {key}")))?;
            out.extend(page.parts.into_iter().map(|p| (p.number, p.etag, p.size)));
            match page.next_part_number_marker {
                Some(m) if marker.is_none_or(|old| m > old) => marker = Some(m),
                Some(_) => return Err(io::Error::other(format!("endless part list for {key}"))),
                None => return Ok(out),
            }
        }
    }

    /// The checksum header's value for `data`, unless the server doesn't take it.
    pub(super) fn sum_header(&self, data: &[u8]) -> Option<String> {
        self.sums
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(|| sum::sha256_b64(data))
    }

    fn add_note(&self, text: &str) {
        let mut n = self.notes.lock().unwrap_or_else(|p| p.into_inner());
        if !n.iter().any(|t| t == text) {
            n.push(text.to_string());
        }
    }

    /// Does a PUT of a whole small object, conditional if `cond` is given, carrying its
    /// checksum and `token`.
    fn put(
        &self,
        key: &str,
        bytes: &[u8],
        cond: Option<(&str, &str)>,
        token: &str,
    ) -> io::Result<Response> {
        let sum = self.sum_header(bytes);
        self.put_with_sum(key, bytes, cond, token, sum.as_deref())
    }

    /// [`put`](Self::put) with the checksum header given (or none).
    fn put_with_sum(
        &self,
        key: &str,
        bytes: &[u8],
        cond: Option<(&str, &str)>,
        token: &str,
        sum: Option<&str>,
    ) -> io::Result<Response> {
        let c = &self.client;
        let mut hdrs: Vec<(&str, &str)> = vec![(H_TOKEN, token)];
        hdrs.extend(sum.map(|s| (H_SUM, s)));
        hdrs.extend(cond);
        let r = c.exec(Method::Put, &hdrs, bytes, |h| {
            Client::presign(c.bucket.put_object(Some(&c.creds), key), h)
        })?;
        Ok(Response(r))
    }

    /// A server-side copy of `from` onto `to` for the self-test: `x-amz-copy-source-if-match`
    /// with `source_tag`, and `If-None-Match: *` if asked.
    fn probe_copy(
        &self,
        from: &str,
        to: &str,
        source_tag: &str,
        if_none_match: bool,
    ) -> io::Result<http::Resp> {
        let c = &self.client;
        let src = copy_source(c.bucket.name(), from);
        let tag = quoted(source_tag);
        let mut hdrs: Vec<(&str, &str)> = vec![
            ("x-amz-copy-source", &src),
            ("x-amz-copy-source-if-match", &tag),
            ("x-amz-metadata-directive", "COPY"),
        ];
        if if_none_match {
            hdrs.push(("if-none-match", "*"));
        }
        hdrs.sort_by_key(|(k, _)| *k);
        c.exec(Method::Put, &hdrs, &[], |h| {
            Client::presign(c.bucket.put_object(Some(&c.creds), to), h)
        })
    }

    /// A crash record's upload reference, checked: one of ours, for a key under the prefix
    /// that is a plain relative path. Nothing is sent.
    fn parse_ref(&self, journal_ref: &str) -> io::Result<UploadRef> {
        let bad = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "that is not an upload of this app",
            )
        };
        if let Some(key) = journal_ref.strip_prefix(PUT_REF) {
            return self.own_key(key).map(|_| UploadRef::Put).ok_or_else(bad);
        }
        let rest = journal_ref.strip_prefix(MPU_REF).ok_or_else(bad)?;
        let (id, key) = rest.split_once(':').ok_or_else(bad)?;
        if id.is_empty() || self.own_key(key).is_none() {
            return Err(bad());
        }
        Ok(UploadRef::Multipart {
            id: id.to_string(),
            key: key.to_string(),
        })
    }

    /// Checks, once, that the server honours conditional writes: under the prefix a probe
    /// object is written, refused a second time (`If-None-Match: *`), refused a write with a
    /// wrong `If-Match`, and deleted. A server that ignores either would let a file be
    /// overwritten, so it is refused. Only a conclusive answer is kept; a failure to reach the
    /// server is returned and asked again next time.
    pub(super) fn probe(&self) -> io::Result<()> {
        let mut seen = self.probed.lock().unwrap_or_else(|p| p.into_inner());
        match *seen {
            Some(true) => return Ok(()),
            Some(false) => return Err(no_conditional_writes()),
            None => {}
        }
        let new_key = || format!("{}{PROBE_PREFIX}{}", self.prefix, sum::new_token());
        let key = new_key();
        let token = sum::new_token();
        let mut first = self
            .put(&key, b"probe", Some(("if-none-match", "*")), &token)?
            .0;
        if self.sums.load(std::sync::atomic::Ordering::Relaxed) && checksum_unsupported(&first) {
            // The server doesn't know the header: uploads go without it.
            warn!("This server doesn't take upload checksums; sending without them");
            self.sums.store(false, std::sync::atomic::Ordering::Relaxed);
            self.add_note(NO_CHECKSUMS);
            first = self
                .put(&key, b"probe", Some(("if-none-match", "*")), &token)?
                .0;
        }
        if matches!(first.status, 401 | 403) {
            // No right to write: nothing to check, and writes would be refused anyway.
            return Ok(());
        }
        if !first.ok() && !(first.status == 412 && first.retried) {
            return Err(self.client.fail(&first, "write the probe object"));
        }
        let tag = self.written_tag(&key, &first).ok();
        // Other probe objects made, to delete afterwards (with the version, where known).
        let mut extra: Vec<(String, Option<Tag>)> = Vec::new();
        let verdict = (|| -> io::Result<bool> {
            let again = self
                .put(&key, b"probe", Some(("if-none-match", "*")), &token)?
                .0;
            if again.ok() {
                return Ok(false);
            }
            if again.status != 412 {
                return Err(self.client.fail(&again, "write the probe object"));
            }
            let wrong = quoted("pnk-no-such-version");
            let stale = self
                .put(&key, b"probe", Some(("if-match", &wrong)), &token)?
                .0;
            if stale.ok() {
                return Ok(false);
            }
            if stale.status != 412 {
                return Err(self.client.fail(&stale, "write the probe object"));
            }
            // Archive moves are server-side copies: both conditions must hold there too.
            let Some(good) = &tag else {
                return Err(io::Error::other(
                    "could not read the version of the probe object",
                ));
            };
            let taken = new_key();
            let made = self
                .put(&taken, b"probe", Some(("if-none-match", "*")), &token)?
                .0;
            if !made.ok() {
                return Err(self.client.fail(&made, "write the probe object"));
            }
            extra.push((taken.clone(), self.written_tag(&taken, &made).ok()));
            let onto = self.probe_copy(&key, &taken, good, true)?;
            if copy_accepted(&onto) {
                return Ok(false);
            }
            if !copy_refused(&onto) {
                return copy_unsupported(&onto)
                    .then_some(false)
                    .ok_or_else(|| self.client.fail(&onto, "copy the probe object"));
            }
            let fresh = new_key();
            extra.push((fresh.clone(), None));
            let stale_src = self.probe_copy(&key, &fresh, "pnk-no-such-version", false)?;
            if copy_accepted(&stale_src) {
                return Ok(false);
            }
            if !copy_refused(&stale_src) {
                return copy_unsupported(&stale_src)
                    .then_some(false)
                    .ok_or_else(|| self.client.fail(&stale_src, "copy the probe object"));
            }
            // Does the server check a checksum it is sent?
            if self.sums.load(std::sync::atomic::Ordering::Relaxed) {
                let bad = new_key();
                let wrong_sum = sum::sha256_b64(b"not the probe");
                let r = self
                    .put_with_sum(
                        &bad,
                        b"probe",
                        Some(("if-none-match", "*")),
                        &token,
                        Some(&wrong_sum),
                    )?
                    .0;
                if r.ok() {
                    extra.push((bad, None));
                    warn!("This server accepts a wrong upload checksum");
                    self.add_note(CHECKSUMS_NOT_CHECKED);
                }
            }
            Ok(true)
        })();
        // The probe objects are ours: delete them, with `If-Match` when they are as written.
        let conditional = matches!(verdict, Ok(true));
        let _ = self.delete(&key, if conditional { tag.as_ref() } else { None });
        for (k, t) in &extra {
            let _ = self.delete(k, if conditional { t.as_ref() } else { None });
        }
        if let Ok(good) = verdict {
            *seen = Some(good);
            if !good {
                return Err(no_conditional_writes());
            }
        }
        verdict.map(|_| ())
    }

    /// The tag a successful write returned, asking the server if it did not say.
    fn written_tag(&self, key: &str, r: &http::Resp) -> io::Result<Tag> {
        match r.header("etag") {
            Some(t) => Ok(normalise_etag(t)),
            None => self
                .head(key)?
                .and_then(|h| h.etag)
                .ok_or_else(|| io::Error::other(format!("{key} vanished after its write"))),
        }
    }
}

struct Response(http::Resp);

/// What a crash record's upload reference names.
enum UploadRef {
    /// A single request upload: nothing outside the request to clean up.
    Put,
    Multipart {
        id: String,
        key: String,
    },
}

/// A write the server refused because it doesn't know the checksum header.
fn checksum_unsupported(r: &http::Resp) -> bool {
    r.status == 501
        || (r.status == 400
            && matches!(
                xml::error_code(&r.text()).as_deref(),
                Some("InvalidArgument" | "NotImplemented")
            ))
}

/// A copy the server carried out.
fn copy_accepted(r: &http::Resp) -> bool {
    r.ok() && !xml::is_error_body(&r.text())
}

/// A copy the server refused on a precondition.
fn copy_refused(r: &http::Resp) -> bool {
    r.status == 412
        || (r.ok() && xml::error_code(&r.text()).as_deref() == Some("PreconditionFailed"))
}

/// A server that has no such copy at all.
fn copy_unsupported(r: &http::Resp) -> bool {
    matches!(r.status, 405 | 501)
}

fn no_conditional_writes() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, NO_CONDITIONAL_WRITES)
}

impl BackupStore for S3Store {
    fn list(&self, prune: &dyn Fn(&str) -> bool) -> io::Result<Listing> {
        let inner = &self.inner;
        let c = &inner.client;
        let mut out = Listing::default();
        let mut verdicts: HashMap<String, bool> = HashMap::new();
        let mut token: Option<String> = None;
        loop {
            let r = c.exec(Method::Get, &[], &[], |h| {
                let mut a = ListObjectsV2::new(&c.bucket, Some(&c.creds));
                if !inner.prefix.is_empty() {
                    a.with_prefix(inner.prefix.clone());
                }
                a.with_max_keys(inner.limits.list_page);
                if let Some(t) = &token {
                    a.with_continuation_token(t.clone());
                }
                Client::presign(a, h)
            })?;
            if !r.ok() {
                return Err(c.fail(&r, "list the bucket"));
            }
            let page: ListObjectsV2Response = ListObjectsV2::parse_response(&r.text())
                .map_err(|_| io::Error::other(format!("unreadable listing from {}", c.endpoint)))?;
            for obj in page.contents {
                let Some(rel) = obj.key.strip_prefix(&inner.prefix) else {
                    continue;
                };
                // Folder placeholders made by other tools.
                if rel.is_empty() || rel.ends_with('/') {
                    continue;
                }
                if SafeRelPath::parse(rel).is_err() {
                    out.issues.push(ListIssue {
                        at: obj.key.clone(),
                        why: "the name can't be used".into(),
                    });
                    continue;
                }
                if pruned(rel, prune, &mut verdicts) {
                    continue;
                }
                out.files.push(Listed {
                    rel: rel.to_string(),
                    size: obj.size.unwrap_or(0),
                    // Not in a listing: the scan gets it from the modified-time cache.
                    mtime: None,
                    tag: obj.etag.as_deref().map(normalise_etag),
                    kind: Kind::File,
                });
            }
            match page.next_continuation_token {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }

    fn list_dirs(&self, under: &str) -> io::Result<Vec<String>> {
        let inner = &self.inner;
        let c = &inner.client;
        let prefix = format!("{}{}", inner.prefix, normalise_prefix(under));
        let mut names = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let r = c.exec(Method::Get, &[], &[], |h| {
                let mut a = ListObjectsV2::new(&c.bucket, Some(&c.creds));
                if !prefix.is_empty() {
                    a.with_prefix(prefix.clone());
                }
                a.with_delimiter("/");
                a.with_max_keys(inner.limits.list_page);
                if let Some(t) = &token {
                    a.with_continuation_token(t.clone());
                }
                Client::presign(a, h)
            })?;
            if !r.ok() {
                return Err(c.fail(&r, "list the bucket"));
            }
            let page: ListObjectsV2Response = ListObjectsV2::parse_response(&r.text())
                .map_err(|_| io::Error::other(format!("unreadable listing from {}", c.endpoint)))?;
            for p in page.common_prefixes {
                if let Some(name) = p.prefix.strip_prefix(&prefix) {
                    let name = name.trim_end_matches('/');
                    if !name.is_empty() && !name.contains('/') {
                        names.push(name.to_string());
                    }
                }
            }
            match page.next_continuation_token {
                Some(t) => token = Some(t),
                None => {
                    names.sort();
                    return Ok(names);
                }
            }
        }
    }

    fn stat(&self, rel: &SafeRelPath) -> io::Result<Option<Stat>> {
        let key = self.inner.key(rel);
        Ok(self.inner.head(&key)?.map(|h| Stat {
            size: h.size,
            mtime: h.mtime_ms.as_deref().and_then(parse_mtime),
            tag: h.etag,
            kind: Kind::File,
        }))
    }

    fn read(&self, rel: &SafeRelPath) -> io::Result<Option<(Vec<u8>, Tag)>> {
        let inner = &self.inner;
        let c = &inner.client;
        let key = inner.key(rel);
        let r = c.exec(Method::Get, &[], &[], |h| {
            Client::presign(c.bucket.get_object(Some(&c.creds), &key), h)
        })?;
        match r.status {
            404 => Ok(None),
            _ if r.ok() => {
                let tag = r.header("etag").map(normalise_etag).unwrap_or_default();
                Ok(Some((r.body, tag)))
            }
            _ => Err(c.fail(&r, &format!("read {key}"))),
        }
    }

    fn write_new(&self, rel: &SafeRelPath, bytes: &[u8]) -> io::Result<Tag> {
        let inner = &self.inner;
        let key = inner.key(rel);
        let token = sum::new_token();
        let Response(r) = inner.put(&key, bytes, Some(("if-none-match", "*")), &token)?;
        match r.status {
            412 => match r
                .retried
                .then(|| inner.own_write(&key, &token, bytes.len() as u64))
            {
                // An earlier try of this very write went through; its answer was lost.
                Some(Some(tag)) => Ok(tag),
                _ => Err(already_exists(&key)),
            },
            _ if r.ok() => inner.written_tag(&key, &r),
            _ => Err(inner.client.fail(&r, &format!("write {key}"))),
        }
    }

    fn write_replace(
        &self,
        rel: &SafeRelPath,
        bytes: &[u8],
        expect: Option<&Tag>,
    ) -> io::Result<Tag> {
        let inner = &self.inner;
        let key = inner.key(rel);
        let token = sum::new_token();
        let Response(r) = match expect {
            None => inner.put(&key, bytes, None, &token)?,
            Some(t) => inner.put(&key, bytes, Some(("if-match", &quoted(t))), &token)?,
        };
        match r.status {
            // The bucket itself is missing: not a changed file.
            404 if xml::error_code(&r.text()).as_deref() == Some("NoSuchBucket") => {
                Err(inner.client.fail(&r, &format!("write {key}")))
            }
            // Changed since read, or gone; unless an earlier try of this very write went
            // through and its answer was lost.
            404 | 409 | 412 if expect.is_some() => {
                match r
                    .retried
                    .then(|| inner.own_write(&key, &token, bytes.len() as u64))
                {
                    Some(Some(tag)) => Ok(tag),
                    _ => Err(super::conflict()),
                }
            }
            _ if r.ok() => inner.written_tag(&key, &r),
            _ => Err(inner.client.fail(&r, &format!("write {key}"))),
        }
    }

    fn begin_upload(
        &self,
        rel: &SafeRelPath,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> io::Result<Box<dyn Upload>> {
        let key = self.inner.key(rel);
        Ok(Box::new(upload::S3Upload::begin(
            self.inner.clone(),
            key,
            size,
            mtime,
        )?))
    }

    fn abort_by_ref(&self, journal_ref: &str) -> io::Result<()> {
        match self.inner.parse_ref(journal_ref)? {
            // One PUT: nothing is ever left half-done.
            UploadRef::Put => Ok(()),
            UploadRef::Multipart { id, key } => self.inner.abort_multipart(&key, &id),
        }
    }

    fn finish_by_ref(&self, journal_ref: &str, expected_size: u64) -> io::Result<Tag> {
        let inner = &self.inner;
        let UploadRef::Multipart { id, key } = inner.parse_ref(journal_ref)? else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a single request upload can't be finished later",
            ));
        };
        let parts = inner.list_parts(&key, &id)?;
        // Whole means: parts 1..=n all there, adding up to the size the record says.
        let numbered = parts
            .iter()
            .enumerate()
            .all(|(i, (n, _, _))| usize::from(*n) == i + 1);
        let total: u64 = parts.iter().map(|(_, _, size)| size).sum();
        if parts.is_empty() || !numbered || total != expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the unfinished upload of {key} is not whole ({} parts, {total} of {expected_size} bytes)",
                    parts.len()
                ),
            ));
        }
        let etags: Vec<String> = parts
            .iter()
            .map(|(_, etag, _)| quoted(&normalise_etag(etag)))
            .collect();
        inner.complete(&key, &id, &etags, None)
    }

    fn move_no_replace(&self, from: &SafeRelPath, to: &SafeRelPath) -> io::Result<()> {
        let inner = &self.inner;
        let (fk, tk) = (inner.key(from), inner.key(to));
        let Some(src) = inner.head(&fk)? else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{from} is not there"),
            ));
        };
        inner.copy(&fk, &tk, &src)?;
        // Only now is it safe to remove the original: check the copy is whole.
        let verified = inner.head(&tk)?.is_some_and(|d| {
            d.size == src.size && d.mtime_ms == src.mtime_ms && d.checksums_agree(&src)
        });
        if !verified {
            return Err(io::Error::other(format!(
                "the copy {to} does not match {from}, so {from} was kept (both exist)"
            )));
        }
        inner.delete(&fk, src.etag.as_ref()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("copied {from} to {to} but could not delete {from}, so both exist: {e}"),
            )
        })
    }

    fn remove_verified_original(&self, from: &SafeRelPath, copy: &SafeRelPath) -> io::Result<bool> {
        let inner = &self.inner;
        let (fk, ck) = (inner.key(from), inner.key(copy));
        let (Some(orig), Some(dup)) = (inner.head(&fk)?, inner.head(&ck)?) else {
            return Ok(false);
        };
        // Only a copy that shows the same modified time counts: without it there is nothing to
        // tell a copy of this file from another file of the same size.
        if orig.size != dup.size
            || orig.mtime_ms.is_none()
            || orig.mtime_ms != dup.mtime_ms
            || !orig.checksums_agree(&dup)
        {
            return Ok(false);
        }
        inner.delete(&fk, orig.etag.as_ref())?;
        Ok(true)
    }

    fn send_to_bin(&self, _rel: &SafeRelPath) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "there is no bin for an S3 bucket",
        ))
    }

    fn supports_bin(&self) -> bool {
        false
    }

    fn space(&self) -> Option<DiskSpace> {
        None
    }

    fn root_exists(&self) -> io::Result<bool> {
        let c = &self.inner.client;
        let r = c.exec(Method::Head, &[], &[], |h| {
            Client::presign(c.bucket.head_bucket(Some(&c.creds)), h)
        })?;
        match r.status {
            404 => Ok(false),
            _ if r.ok() => self.inner.probe().map(|()| true),
            _ => Err(c.fail(&r, &format!("open bucket {}", c.bucket.name()))),
        }
    }

    fn ensure_root(&self) -> io::Result<()> {
        if self.root_exists()? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "bucket {} does not exist on {}",
                    self.inner.client.bucket.name(),
                    self.inner.client.endpoint
                ),
            ))
        }
    }

    fn destination(&self) -> Option<Destination> {
        Some(Destination::S3(self.config.clone()))
    }

    fn server_note(&self) -> Option<String> {
        let n = self.inner.notes.lock().unwrap_or_else(|p| p.into_inner());
        (!n.is_empty()).then(|| n.join(" "))
    }

    fn describe(&self) -> String {
        format!(
            "s3://{}/{} on {}",
            self.inner.client.bucket.name(),
            self.inner.prefix,
            self.inner.client.endpoint
        )
    }

    fn clean_stale_uploads(&self, older_than: Duration) -> io::Result<usize> {
        self.clean_stale(older_than)
    }
}

/// Whether any folder above `rel` is pruned, asking `prune` once per folder.
pub(super) fn pruned(
    rel: &str,
    prune: &dyn Fn(&str) -> bool,
    verdicts: &mut HashMap<String, bool>,
) -> bool {
    let mut at = 0;
    while let Some(i) = rel[at..].find('/') {
        at += i;
        let dir = &rel[..at];
        let v = *verdicts
            .entry(dir.to_string())
            .or_insert_with(|| prune(dir));
        if v {
            return true;
        }
        at += 1;
    }
    false
}
