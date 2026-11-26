//! One signed HTTP call at a time: rusty-s3 signs, ureq sends. Retries, timeouts and the
//! mapping of failures to `io::Error` live here. Error texts never carry a signed address (it
//! holds the access key id and the signature): only the endpoint and fixed words.
#![forbid(unsafe_code)]

use std::io;
use std::path::Path;
use std::time::Duration;

use rusty_s3::{Bucket, Credentials, Method, S3Action, UrlStyle};
use ureq::http::HeaderMap;
use url::Url;

use super::xml;
use crate::store::{Offline, conflict};

const SIGN_FOR: Duration = Duration::from_secs(15 * 60);
const TRIES: u32 = 3;

pub(super) struct Client {
    agent: ureq::Agent,
    pub(super) bucket: Bucket,
    pub(super) creds: Credentials,
    /// For messages.
    pub(super) endpoint: String,
    /// The wait before the second try; it grows fourfold for each one after.
    pub(super) backoff: Duration,
    /// Tests answer requests themselves instead of using the network.
    #[cfg(test)]
    pub(super) fake: Option<Fake>,
}

/// A stand-in for the network: gets the method, the signed address, the headers and the body.
#[cfg(test)]
pub(super) type Fake = std::sync::Arc<
    dyn Fn(Method, &Url, &[(&str, &str)], &[u8]) -> Result<Resp, Failure> + Send + Sync,
>;

pub(super) struct Resp {
    pub status: u16,
    headers: HeaderMap,
    pub body: Vec<u8>,
    /// This answer came after an earlier try that got none, so that try may have been carried
    /// out: a conditional write that now says "already there" may be answering itself.
    pub retried: bool,
}

impl Resp {
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// An answer made by a test.
    #[cfg(test)]
    pub(super) fn fake(status: u16, headers: HeaderMap, body: Vec<u8>) -> Resp {
        Resp {
            status,
            headers,
            body,
            retried: false,
        }
    }

    pub(super) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl Client {
    pub(super) fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        path_style: bool,
        creds: &crate::store::creds::Credentials,
        ca_file: Option<&Path>,
    ) -> Result<Self, String> {
        let url: Url = endpoint
            .trim()
            .parse()
            .map_err(|_| format!("\"{endpoint}\" is not a valid S3 address"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(format!(
                "\"{endpoint}\" is not a valid S3 address (it should start with http:// or https://)"
            ));
        }
        let style = if path_style {
            UrlStyle::Path
        } else {
            UrlStyle::VirtualHost
        };
        let bucket_ = Bucket::new(url, style, bucket.to_string(), region.to_string())
            .map_err(|e| format!("bad S3 bucket settings: {e}"))?;
        let creds_ = match &creds.session_token {
            Some(t) => Credentials::new_with_token(
                creds.access_key.clone(),
                creds.secret_key.clone(),
                t.clone(),
            ),
            None => Credentials::new(creds.access_key.clone(), creds.secret_key.clone()),
        };
        let mut tls = ureq::tls::TlsConfig::builder();
        if let Some(path) = ca_file {
            let pem = std::fs::read(path)
                .map_err(|e| format!("can't read the CA file {}: {}", path.display(), e.kind()))?;
            let certs = parse_pem_certs(&pem)
                .map_err(|why| format!("the CA file {} {why}", path.display()))?;
            tls = tls.root_certs(ureq::tls::RootCerts::new_with_certs(&certs));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_resolve(Some(Duration::from_secs(10)))
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_per_read(Some(Duration::from_secs(120)))
            .timeout_per_write(Some(Duration::from_secs(120)))
            .tls_config(tls.build())
            .build()
            .into();
        Ok(Client {
            agent,
            bucket: bucket_,
            creds: creds_,
            endpoint: endpoint.trim().trim_end_matches('/').to_string(),
            backoff: Duration::from_millis(400),
            #[cfg(test)]
            fake: None,
        })
    }

    /// Signs with `sign` (given the headers to sign) and sends, trying again on a timeout, a
    /// dropped connection or a 5xx. Any HTTP answer, whatever its status, is returned; only a
    /// failure to get one is an error.
    pub(super) fn exec(
        &self,
        method: Method,
        headers: &[(&str, &str)],
        body: &[u8],
        sign: impl for<'h> Fn(&'h [(&'h str, &'h str)]) -> Url,
    ) -> io::Result<Resp> {
        let mut wait = self.backoff;
        let mut last: Option<io::Result<Resp>> = None;
        for attempt in 0..TRIES {
            if attempt > 0 {
                std::thread::sleep(wait);
                wait *= 4;
            }
            let url = sign(headers);
            match self.once(method, &url, headers, body) {
                Ok(mut r) if r.status >= 500 || r.status == 429 => {
                    r.retried = attempt > 0;
                    last = Some(Ok(r));
                }
                Ok(mut r) => {
                    r.retried = attempt > 0;
                    return Ok(r);
                }
                Err(Failure::Retry(e)) => last = Some(Err(e)),
                Err(Failure::Fatal(e)) => return Err(e),
            }
        }
        last.expect("at least one try was made")
    }

    /// Signs an action with the given headers added.
    pub(super) fn presign<'a, A: S3Action<'a>>(
        mut action: A,
        headers: &'a [(&'a str, &'a str)],
    ) -> Url {
        for (k, v) in headers {
            action.headers_mut().insert(*k, *v);
        }
        action.sign(SIGN_FOR)
    }

    fn once(
        &self,
        method: Method,
        url: &Url,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Resp, Failure> {
        #[cfg(test)]
        if let Some(fake) = &self.fake {
            return fake(method, url, headers, body);
        }
        let uri = url.as_str();
        let result = match method {
            Method::Get | Method::Head | Method::Delete => {
                let mut r = match method {
                    Method::Get => self.agent.get(uri),
                    Method::Head => self.agent.head(uri),
                    _ => self.agent.delete(uri),
                };
                for (k, v) in headers {
                    r = r.header(*k, *v);
                }
                r.call()
            }
            Method::Put | Method::Post => {
                let mut r = if method == Method::Put {
                    self.agent.put(uri)
                } else {
                    self.agent.post(uri)
                };
                for (k, v) in headers {
                    r = r.header(*k, *v);
                }
                r.send(body)
            }
        };
        let mut resp = result.map_err(|e| self.transport_error(e))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = if method == Method::Head {
            Vec::new()
        } else {
            resp.body_mut()
                .with_config()
                .limit(u64::MAX)
                .read_to_vec()
                .map_err(|e| self.transport_error(e))?
        };
        Ok(Resp {
            status,
            headers,
            body,
            retried: false,
        })
    }

    fn transport_error(&self, e: ureq::Error) -> Failure {
        classify_transport(&e, &self.endpoint)
    }

    /// The error for an HTTP answer that was not a success. `what` says what was being done,
    /// e.g. `get a/b.jpg`.
    pub(super) fn fail(&self, r: &Resp, what: &str) -> io::Error {
        let code = xml::error_code(&r.text());
        let denied = matches!(r.status, 401 | 403);
        let msg = format!(
            "S3 could not {what} on {}: {}HTTP {}{}",
            self.endpoint,
            if denied {
                "no access to the bucket, "
            } else {
                ""
            },
            r.status,
            code.as_deref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default()
        );
        match kind_for_status(r.status) {
            // A full server or a quota is a full disk to the sync.
            _ if is_full(r.status, code.as_deref()) => {
                io::Error::new(io::ErrorKind::StorageFull, msg)
            }
            Mapped::Conflict => conflict(),
            Mapped::Kind(k) => io::Error::new(k, msg),
        }
    }
}

/// Whether a failing answer means the server has no room (insufficient storage, a quota, or an
/// object bigger than the server takes).
fn is_full(status: u16, code: Option<&str>) -> bool {
    status == 507
        || matches!(
            code,
            Some(
                "QuotaExceeded"
                    | "EntityTooLarge"
                    | "XMinioStorageFull"
                    | "InsufficientStorage"
                    | "StorageFull"
            )
        )
}

pub(super) enum Failure {
    Retry(io::Error),
    Fatal(io::Error),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Mapped {
    Kind(io::ErrorKind),
    /// A conditional write lost (409, 412): see `store::is_conflict`.
    Conflict,
}

/// How a failing HTTP status is reported by default. Callers that give 404 or 412 a meaning of
/// their own look at the status first.
pub(super) fn kind_for_status(status: u16) -> Mapped {
    match status {
        404 => Mapped::Kind(io::ErrorKind::NotFound),
        401 | 403 => Mapped::Kind(io::ErrorKind::PermissionDenied),
        409 | 412 => Mapped::Conflict,
        400 | 405 | 411 | 413 | 416 | 422 => Mapped::Kind(io::ErrorKind::InvalidInput),
        408 => Mapped::Kind(io::ErrorKind::TimedOut),
        501 => Mapped::Kind(io::ErrorKind::Unsupported),
        _ => Mapped::Kind(io::ErrorKind::Other),
    }
}

fn offline(kind: io::ErrorKind, endpoint: &str, why: &str) -> Failure {
    Failure::Fatal(io::Error::new(
        kind,
        Offline(format!("can't reach {endpoint}: {why}")),
    ))
}

fn classify_transport(e: &ureq::Error, endpoint: &str) -> Failure {
    use io::ErrorKind as K;
    use ureq::Error as E;
    match e {
        E::HostNotFound => offline(K::NotFound, endpoint, "the server name was not found"),
        E::ConnectionFailed => offline(K::ConnectionRefused, endpoint, "the connection failed"),
        E::Timeout(ureq::Timeout::Resolve | ureq::Timeout::Connect) => {
            offline(K::TimedOut, endpoint, "no answer")
        }
        E::Timeout(_) => Failure::Retry(io::Error::new(
            K::TimedOut,
            format!("the server at {endpoint} took too long to answer"),
        )),
        E::Io(io) => match io.kind() {
            K::ConnectionRefused
            | K::NotConnected
            | K::AddrNotAvailable
            | K::HostUnreachable
            | K::NetworkUnreachable
            | K::NetworkDown => offline(io.kind(), endpoint, "the connection was refused"),
            K::TimedOut
            | K::ConnectionReset
            | K::ConnectionAborted
            | K::BrokenPipe
            | K::UnexpectedEof => Failure::Retry(io::Error::new(
                io.kind(),
                format!("the connection to {endpoint} dropped"),
            )),
            k => Failure::Fatal(io::Error::new(
                k,
                format!("network problem with {endpoint}"),
            )),
        },
        E::Tls(_) | E::Pem(_) | E::Rustls(_) => Failure::Fatal(io::Error::other(format!(
            "secure connection to {endpoint} failed (certificate or TLS problem)"
        ))),
        // The error's own text may hold the signed address, so only its kind is shown.
        _ => Failure::Fatal(io::Error::other(format!(
            "request to {endpoint} failed ({})",
            error_kind(e)
        ))),
    }
}

/// A fixed word for what sort of error `e` is.
fn error_kind(e: &ureq::Error) -> &'static str {
    use ureq::Error as E;
    match e {
        E::Http(_) | E::BadUri(_) => "bad request",
        E::Protocol(_) | E::LargeResponseHeader(..) => "protocol error",
        E::BodyExceedsLimit(_) | E::BodyStalled => "body problem",
        E::RedirectFailed | E::TooManyRedirects => "redirect",
        E::RequireHttpsOnly(_) | E::TlsRequired => "https required",
        E::StatusCode(_) => "status",
        _ => "other",
    }
}

/// Every certificate in a PEM file.
fn parse_pem_certs(pem: &[u8]) -> Result<Vec<ureq::tls::Certificate<'static>>, &'static str> {
    const END: &str = "-----END CERTIFICATE-----";
    let text = std::str::from_utf8(pem).map_err(|_| "is not text")?;
    let mut certs = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(END) {
        let block = &rest[..i + END.len()];
        rest = &rest[i + END.len()..];
        certs.push(
            ureq::tls::Certificate::from_pem(block.as_bytes())
                .map_err(|_| "has a certificate that can't be read")?,
        );
    }
    if certs.is_empty() {
        return Err("has no certificate in it");
    }
    Ok(certs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{is_conflict, is_offline};

    #[test]
    fn statuses_map_to_kinds() {
        use io::ErrorKind as K;
        assert_eq!(kind_for_status(404), Mapped::Kind(K::NotFound));
        assert_eq!(kind_for_status(403), Mapped::Kind(K::PermissionDenied));
        assert_eq!(kind_for_status(412), Mapped::Conflict);
        assert_eq!(kind_for_status(409), Mapped::Conflict);
        assert_eq!(kind_for_status(500), Mapped::Kind(K::Other));
        assert_eq!(kind_for_status(400), Mapped::Kind(K::InvalidInput));
    }

    #[test]
    fn failures_to_connect_are_offline() {
        for e in [
            ureq::Error::HostNotFound,
            ureq::Error::ConnectionFailed,
            ureq::Error::Timeout(ureq::Timeout::Connect),
            ureq::Error::Io(io::Error::from(io::ErrorKind::ConnectionRefused)),
        ] {
            match classify_transport(&e, "http://x:1") {
                Failure::Fatal(err) => assert!(is_offline(&err), "{err}"),
                Failure::Retry(_) => panic!("should not retry"),
            }
        }
        match classify_transport(&ureq::Error::Timeout(ureq::Timeout::RecvBody), "http://x") {
            Failure::Retry(e) => {
                assert!(!is_offline(&e));
                assert_eq!(e.kind(), io::ErrorKind::TimedOut);
            }
            Failure::Fatal(_) => panic!("should retry"),
        }
        assert!(!is_conflict(&io::Error::other("x")));
    }

    #[test]
    fn a_full_server_and_a_missing_right_are_told_apart() {
        let c = crate::store::creds::Credentials {
            access_key: "k".into(),
            secret_key: "s".into(),
            session_token: None,
        };
        let client = Client::new("http://127.0.0.1:9", "r", "b", true, &c, None).unwrap();
        let answer = |status: u16, code: &str| Resp {
            status,
            headers: HeaderMap::new(),
            body: format!("<Error><Code>{code}</Code></Error>").into_bytes(),
            retried: false,
        };
        for (status, code) in [
            (507, ""),
            (403, "QuotaExceeded"),
            (400, "EntityTooLarge"),
            (500, "XMinioStorageFull"),
        ] {
            let e = client.fail(&answer(status, code), "put x");
            assert_eq!(e.kind(), io::ErrorKind::StorageFull, "{status} {code}");
        }
        let e = client.fail(&answer(403, "AccessDenied"), "put x");
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("no access to the bucket"), "{e}");
        assert_eq!(
            client.fail(&answer(500, "InternalError"), "x").kind(),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn error_texts_never_carry_the_signed_address() {
        let signed = "http://h/b/k?X-Amz-Credential=AKIAKEYID%2Fx&X-Amz-Signature=deadbeef";
        for e in [
            ureq::Error::BadUri(signed.into()),
            ureq::Error::RequireHttpsOnly(signed.into()),
            ureq::Error::Other(signed.into()),
            ureq::Error::ConnectProxyFailed(signed.into()),
        ] {
            let Failure::Fatal(err) = classify_transport(&e, "http://h") else {
                panic!("should not retry");
            };
            let text = err.to_string();
            assert!(text.contains("http://h"), "{text}");
            for secret in ["AKIAKEYID", "deadbeef", "X-Amz"] {
                assert!(!text.contains(secret), "{text}");
            }
        }
    }

    #[test]
    fn bad_addresses_are_refused() {
        let c = crate::store::creds::Credentials {
            access_key: "k".into(),
            secret_key: "s".into(),
            session_token: None,
        };
        assert!(Client::new("nonsense", "r", "b", true, &c, None).is_err());
        assert!(Client::new("ftp://host", "r", "b", true, &c, None).is_err());
        assert!(Client::new("http://127.0.0.1:9", "r", "b", true, &c, None).is_ok());
    }
}
