//! A pretend S3 server for tests that need what a real one will not do on demand: lose the
//! answer to a request that was carried out, refuse a request, or let the test see the order of
//! requests. It answers the signed requests the store makes, in memory, path style, bucket `b`.
//! It checks preconditions and `x-amz-checksum-sha256` like the real test server does.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use rusty_s3::Method;
use ureq::http::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

use super::http::{Failure, Fake, Resp};
use super::{sum, xml};

const NS: &str = "xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"";

struct Obj {
    data: Vec<u8>,
    etag: String,
    meta: BTreeMap<String, String>,
    checksum: Option<String>,
}

struct Upl {
    key: String,
    meta: BTreeMap<String, String>,
    parts: BTreeMap<u16, (Vec<u8>, String)>,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Obj>,
    uploads: BTreeMap<String, Upl>,
    counter: u64,
    log: Vec<String>,
    /// Requests whose answer is lost after they were carried out (by the first word of their
    /// log line).
    lose: Vec<String>,
    /// Requests answered 503 without being carried out.
    refuse: Vec<String>,
    ignore_conditions: bool,
    /// Only a copy's preconditions are ignored.
    ignore_copy_conditions: bool,
    /// `x-amz-checksum-sha256` is answered 400 InvalidArgument.
    reject_checksums: bool,
    /// A wrong `x-amz-checksum-sha256` is accepted.
    skip_checksum_check: bool,
    corrupt_next_body: bool,
    bucket_missing: bool,
    head_without_length: bool,
}

#[derive(Clone, Default)]
pub(crate) struct FakeS3 {
    inner: Arc<Mutex<State>>,
}

fn answer(status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) -> Resp {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        map.insert(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    Resp::fake(status, map, body.into())
}

fn error(status: u16, code: &str) -> Resp {
    answer(
        status,
        &[],
        format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>m</Message></Error>"),
    )
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl FakeS3 {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn transport(&self) -> Fake {
        let me = self.clone();
        Arc::new(move |method, url, headers, body| me.handle(method, url, headers, body))
    }

    /// The next request starting with `op` (`put`, `part`, `create`, `complete`, `copy`, ...)
    /// is carried out, but its answer is lost.
    pub(crate) fn lose_reply(&self, op: &str) {
        self.lock().lose.push(op.to_string());
    }

    /// The next request starting with `op` is answered 503 and not carried out.
    pub(crate) fn refuse_once(&self, op: &str) {
        self.lock().refuse.push(op.to_string());
    }

    /// Preconditions (`If-Match`, `If-None-Match`) are ignored, as a server that lacks them does.
    pub(crate) fn ignore_conditions(&self) {
        self.lock().ignore_conditions = true;
    }

    /// A copy's `If-None-Match` and `x-amz-copy-source-if-match` are ignored.
    pub(crate) fn ignore_copy_conditions(&self) {
        self.lock().ignore_copy_conditions = true;
    }

    /// A server that does not know `x-amz-checksum-sha256`: 400 InvalidArgument.
    pub(crate) fn reject_checksums(&self) {
        self.lock().reject_checksums = true;
    }

    /// A server that takes `x-amz-checksum-sha256` but never checks it.
    pub(crate) fn skip_checksum_check(&self) {
        self.lock().skip_checksum_check = true;
    }

    /// The next body that arrives has one byte changed before it is checked.
    pub(crate) fn corrupt_next_body(&self) {
        self.lock().corrupt_next_body = true;
    }

    /// Every request is answered "no such bucket".
    pub(crate) fn missing_bucket(&self) {
        self.lock().bucket_missing = true;
    }

    /// HEAD answers carry no `content-length`.
    pub(crate) fn head_without_length(&self) {
        self.lock().head_without_length = true;
    }

    pub(crate) fn log(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    pub(crate) fn count(&self, op: &str) -> usize {
        self.log()
            .iter()
            .filter(|l| l.split(' ').next() == Some(op))
            .count()
    }

    pub(crate) fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().objects.get(key).map(|o| o.data.clone())
    }

    pub(crate) fn keys(&self) -> Vec<String> {
        self.lock().objects.keys().cloned().collect()
    }

    pub(crate) fn open_uploads(&self) -> Vec<String> {
        self.lock().uploads.keys().cloned().collect()
    }

    /// An object put in directly, with user metadata (`pnk-mtime` and so on).
    pub(crate) fn insert(&self, key: &str, data: &[u8], meta: &[(&str, &str)]) {
        let mut g = self.lock();
        g.counter += 1;
        let etag = format!("\"e{}\"", g.counter);
        let meta = meta
            .iter()
            .map(|(k, v)| (format!("x-amz-meta-{k}"), v.to_string()))
            .collect();
        g.objects.insert(
            key.to_string(),
            Obj {
                data: data.to_vec(),
                etag,
                meta,
                checksum: None,
            },
        );
    }

    /// An unfinished upload put in directly (a part of 1 byte).
    pub(crate) fn insert_upload(&self, key: &str, id: &str) {
        self.lock().uploads.insert(
            id.to_string(),
            Upl {
                key: key.to_string(),
                meta: BTreeMap::new(),
                parts: BTreeMap::from([(1, (vec![1], "\"p\"".to_string()))]),
            },
        );
    }

    fn handle(
        &self,
        method: Method,
        url: &Url,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Resp, Failure> {
        let mut g = self.lock();
        let hdr = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| *v)
        };
        let path = decode(url.path());
        let q: HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let key = path
            .strip_prefix("/b/")
            .map(str::to_string)
            .unwrap_or_default();
        let label = match method {
            Method::Head if key.is_empty() => "head-bucket".to_string(),
            Method::Head => format!("head {key}"),
            Method::Get if q.contains_key("uploads") => "uploads".to_string(),
            Method::Get if q.contains_key("uploadId") => format!("parts {key}"),
            Method::Get if q.contains_key("list-type") => "list".to_string(),
            Method::Get => format!("get {key}"),
            Method::Delete if q.contains_key("uploadId") => format!("abort {key}"),
            Method::Delete => format!("delete {key}"),
            Method::Post if q.contains_key("uploads") => format!("create {key}"),
            Method::Post => format!("complete {key}"),
            Method::Put if q.contains_key("partNumber") => {
                format!("part {key} {}", q["partNumber"])
            }
            Method::Put if hdr("x-amz-copy-source").is_some() => format!(
                "copy {}->{key}",
                decode(hdr("x-amz-copy-source").unwrap_or_default()).trim_start_matches("/b/")
            ),
            Method::Put => format!("put {key}"),
        };
        g.log.push(label.clone());
        let op = label.split(' ').next().unwrap_or_default().to_string();
        if g.bucket_missing {
            return Ok(error(404, "NoSuchBucket"));
        }
        if let Some(i) = g.refuse.iter().position(|o| *o == op) {
            g.refuse.remove(i);
            return Ok(error(503, "SlowDown"));
        }
        let resp = self.carry_out(&mut g, method, &q, &key, headers, body);
        if let Some(i) = g.lose.iter().position(|o| *o == op) {
            g.lose.remove(i);
            return Err(Failure::Retry(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "the connection dropped",
            )));
        }
        Ok(resp)
    }

    fn carry_out(
        &self,
        g: &mut State,
        method: Method,
        q: &HashMap<String, String>,
        key: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Resp {
        let hdr = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| *v)
        };
        let conditions = !g.ignore_conditions;
        let copy_conditions = conditions && !g.ignore_copy_conditions;
        if g.reject_checksums && hdr("x-amz-checksum-sha256").is_some() {
            return error(400, "InvalidArgument");
        }
        let mut body = body.to_vec();
        if std::mem::take(&mut g.corrupt_next_body) && !body.is_empty() {
            body[0] ^= 0xff;
        }
        let meta: BTreeMap<String, String> = ["x-amz-meta-pnk-mtime", "x-amz-meta-pnk-token"]
            .iter()
            .filter_map(|k| hdr(k).map(|v| (k.to_string(), v.to_string())))
            .collect();
        let skip_sum = g.skip_checksum_check;
        let sum_ok = |data: &[u8]| {
            skip_sum || hdr("x-amz-checksum-sha256").is_none_or(|s| s == sum::sha256_b64(data))
        };
        let obj_headers = |o: &Obj, with_len: bool| {
            let mut v: Vec<(String, String)> = vec![("etag".into(), o.etag.clone())];
            if with_len {
                v.push(("content-length".into(), o.data.len().to_string()));
            }
            v.extend(o.meta.iter().map(|(k, v)| (k.clone(), v.clone())));
            if hdr("x-amz-checksum-mode").is_some()
                && let Some(c) = &o.checksum
            {
                v.push(("x-amz-checksum-sha256".into(), c.clone()));
            }
            v
        };
        match method {
            Method::Head if key.is_empty() => answer(200, &[], ""),
            Method::Head => match g.objects.get(key) {
                None => answer(404, &[], ""),
                Some(o) => {
                    let h = obj_headers(o, !g.head_without_length);
                    let h: Vec<(&str, &str)> =
                        h.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    answer(200, &h, "")
                }
            },
            Method::Get if q.contains_key("list-type") => {
                let prefix = q.get("prefix").cloned().unwrap_or_default();
                // With a delimiter, keys with one after the prefix are folded into a folder name.
                let delim = q.get("delimiter").cloned();
                let folded = |k: &str| {
                    let d = delim.as_deref()?;
                    let rest = &k[prefix.len()..];
                    rest.find(d)
                        .map(|i| format!("{prefix}{}", &rest[..i + d.len()]))
                };
                let folders: std::collections::BTreeSet<String> = g
                    .objects
                    .keys()
                    .filter(|k| k.starts_with(&prefix))
                    .filter_map(|k| folded(k))
                    .collect();
                let folders: String = folders
                    .iter()
                    .map(|f| format!("<CommonPrefixes><Prefix>{f}</Prefix></CommonPrefixes>"))
                    .collect();
                let items: String = g
                    .objects
                    .iter()
                    .filter(|(k, _)| k.starts_with(&prefix) && folded(k).is_none())
                    .map(|(k, o)| {
                        format!(
                            "<Contents><Key>{k}</Key><LastModified>2020-01-01T00:00:00.000Z</LastModified><ETag>{}</ETag><Size>{}</Size></Contents>",
                            o.etag.replace('"', "&quot;"),
                            o.data.len()
                        )
                    })
                    .collect();
                answer(
                    200,
                    &[],
                    format!(
                        "<ListBucketResult {NS}><Name>b</Name><IsTruncated>false</IsTruncated>{items}{folders}</ListBucketResult>"
                    ),
                )
            }
            Method::Get if q.contains_key("uploads") => {
                let prefix = q.get("prefix").cloned().unwrap_or_default();
                let items: String = g
                    .uploads
                    .iter()
                    .filter(|(_, u)| u.key.starts_with(&prefix))
                    .map(|(id, u)| {
                        format!(
                            "<Upload><Key>{}</Key><UploadId>{id}</UploadId><Initiated>2020-01-01T00:00:00.000Z</Initiated></Upload>",
                            u.key
                        )
                    })
                    .collect();
                answer(
                    200,
                    &[],
                    format!(
                        "<ListMultipartUploadsResult {NS}><IsTruncated>false</IsTruncated>{items}</ListMultipartUploadsResult>"
                    ),
                )
            }
            Method::Get if q.contains_key("uploadId") => match g.uploads.get(&q["uploadId"]) {
                None => error(404, "NoSuchUpload"),
                Some(u) => {
                    let items: String = u
                        .parts
                        .iter()
                        .map(|(n, (d, e))| {
                            format!(
                                "<Part><PartNumber>{n}</PartNumber><ETag>{}</ETag><LastModified>2020-01-01T00:00:00.000Z</LastModified><Size>{}</Size></Part>",
                                e.replace('"', "&quot;"),
                                d.len()
                            )
                        })
                        .collect();
                    answer(
                        200,
                        &[],
                        format!(
                            "<ListPartsResult {NS}><MaxParts>1000</MaxParts><IsTruncated>false</IsTruncated>{items}</ListPartsResult>"
                        ),
                    )
                }
            },
            Method::Get => match g.objects.get(key) {
                None => error(404, "NoSuchKey"),
                Some(o) => {
                    let h = obj_headers(o, false);
                    let h: Vec<(&str, &str)> =
                        h.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    answer(200, &h, o.data.clone())
                }
            },
            Method::Delete if q.contains_key("uploadId") => {
                if g.uploads.remove(&q["uploadId"]).is_some() {
                    answer(204, &[], "")
                } else {
                    error(404, "NoSuchUpload")
                }
            }
            Method::Delete => {
                match (g.objects.get(key), hdr("if-match")) {
                    (None, _) => return error(404, "NoSuchKey"),
                    (Some(o), Some(m)) if conditions && m != o.etag => {
                        return error(412, "PreconditionFailed");
                    }
                    _ => {}
                }
                g.objects.remove(key);
                answer(204, &[], "")
            }
            Method::Post if q.contains_key("uploads") => {
                g.counter += 1;
                let id = format!("up{}", g.counter);
                g.uploads.insert(
                    id.clone(),
                    Upl {
                        key: key.to_string(),
                        meta,
                        parts: BTreeMap::new(),
                    },
                );
                answer(
                    200,
                    &[],
                    format!(
                        "<InitiateMultipartUploadResult {NS}><Bucket>b</Bucket><Key>{key}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
                    ),
                )
            }
            Method::Post => {
                let id = q.get("uploadId").cloned().unwrap_or_default();
                if !g.uploads.contains_key(&id) {
                    return error(404, "NoSuchUpload");
                }
                if conditions && hdr("if-none-match") == Some("*") && g.objects.contains_key(key) {
                    return error(412, "PreconditionFailed");
                }
                let text = String::from_utf8_lossy(&body).into_owned();
                let mut data = Vec::new();
                let mut rest = text.as_str();
                let mut count = 0;
                while let (Some(n), Some(e)) = (
                    xml::tag(rest, "PartNumber").and_then(|n| n.parse::<u16>().ok()),
                    xml::tag(rest, "ETag").map(xml::unescape),
                ) {
                    let up = &g.uploads[&id];
                    match up.parts.get(&n) {
                        Some((d, have)) if *have == e => data.extend_from_slice(d),
                        _ => return error(400, "InvalidPart"),
                    }
                    count += 1;
                    rest = &rest[rest.find("</Part>").map_or(rest.len(), |i| i + 7)..];
                }
                let up = g.uploads.remove(&id).expect("checked");
                g.counter += 1;
                let etag = format!("\"m{}-{count}\"", g.counter);
                g.objects.insert(
                    key.to_string(),
                    Obj {
                        data,
                        etag: etag.clone(),
                        meta: up.meta,
                        checksum: None,
                    },
                );
                answer(
                    200,
                    &[],
                    format!(
                        "<?xml version=\"1.0\"?><CompleteMultipartUploadResult {NS}><Location>x</Location><Bucket>b</Bucket><Key>{key}</Key><ETag>{}</ETag></CompleteMultipartUploadResult>",
                        etag.replace('"', "&quot;")
                    ),
                )
            }
            Method::Put if q.contains_key("partNumber") && hdr("x-amz-copy-source").is_some() => {
                let id = q.get("uploadId").cloned().unwrap_or_default();
                let src = decode(hdr("x-amz-copy-source").unwrap_or_default());
                let Some(o) = g.objects.get(src.trim_start_matches("/b/")) else {
                    return error(404, "NoSuchKey");
                };
                if conditions
                    && let Some(m) = hdr("x-amz-copy-source-if-match")
                    && m != o.etag
                {
                    return error(412, "PreconditionFailed");
                }
                let range = hdr("x-amz-copy-source-range")
                    .and_then(|r| r.strip_prefix("bytes="))
                    .and_then(|r| r.split_once('-'))
                    .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)));
                let data = match range {
                    Some((a, b)) => o.data[a..=b.min(o.data.len() - 1)].to_vec(),
                    None => o.data.clone(),
                };
                let n: u16 = q["partNumber"].parse().unwrap_or(0);
                let etag = format!(
                    "\"{}\"",
                    sum::sha256_b64(&data).replace(['/', '+', '='], "x")
                );
                let Some(up) = g.uploads.get_mut(&id) else {
                    return error(404, "NoSuchUpload");
                };
                up.parts.insert(n, (data, etag.clone()));
                answer(
                    200,
                    &[],
                    format!(
                        "<CopyPartResult><ETag>{}</ETag></CopyPartResult>",
                        etag.replace('"', "&quot;")
                    ),
                )
            }
            Method::Put if q.contains_key("partNumber") => {
                let id = q.get("uploadId").cloned().unwrap_or_default();
                if !sum_ok(&body) {
                    return error(400, "BadDigest");
                }
                let n: u16 = q["partNumber"].parse().unwrap_or(0);
                let Some(up) = g.uploads.get_mut(&id) else {
                    return error(404, "NoSuchUpload");
                };
                let etag = format!(
                    "\"{}\"",
                    sum::sha256_b64(&body).replace(['/', '+', '='], "x")
                );
                up.parts.insert(n, (body, etag.clone()));
                answer(200, &[("etag", &etag)], "")
            }
            Method::Put => {
                let exists = g.objects.get(key);
                let is_copy = hdr("x-amz-copy-source").is_some();
                if conditions && !(is_copy && !copy_conditions) {
                    if hdr("if-none-match") == Some("*") && exists.is_some() {
                        return error(412, "PreconditionFailed");
                    }
                    if let Some(m) = hdr("if-match") {
                        match exists {
                            None => return error(404, "NoSuchKey"),
                            Some(o) if o.etag != m => return error(412, "PreconditionFailed"),
                            _ => {}
                        }
                    }
                }
                let (data, mut meta, checksum) = match hdr("x-amz-copy-source") {
                    Some(src) => {
                        let src = decode(src);
                        let src = src.trim_start_matches("/b/");
                        let Some(o) = g.objects.get(src) else {
                            return error(404, "NoSuchKey");
                        };
                        if copy_conditions
                            && let Some(m) = hdr("x-amz-copy-source-if-match")
                            && m != o.etag
                        {
                            return error(412, "PreconditionFailed");
                        }
                        (o.data.clone(), o.meta.clone(), o.checksum.clone())
                    }
                    None => {
                        if !sum_ok(&body) {
                            return error(400, "BadDigest");
                        }
                        (body, meta, hdr("x-amz-checksum-sha256").map(str::to_string))
                    }
                };
                meta.retain(|k, _| k.starts_with("x-amz-meta-"));
                g.counter += 1;
                let etag = format!("\"e{}\"", g.counter);
                g.objects.insert(
                    key.to_string(),
                    Obj {
                        data,
                        etag: etag.clone(),
                        meta,
                        checksum,
                    },
                );
                if hdr("x-amz-copy-source").is_some() {
                    answer(
                        200,
                        &[],
                        format!(
                            "<CopyObjectResult><ETag>{}</ETag></CopyObjectResult>",
                            etag.replace('"', "&quot;")
                        ),
                    )
                } else {
                    answer(200, &[("etag", &etag)], "")
                }
            }
        }
    }
}
