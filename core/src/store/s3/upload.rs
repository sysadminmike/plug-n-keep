//! A file being sent to S3. A small file is held in memory and sent with one PUT at the finish,
//! unless it is staged (a changed file, whose old copy moves away before the finish): then it
//! goes up as a one-part multipart upload at once and the finish only completes it. Bigger
//! files always go up as a multipart upload, one part at a time (so memory is about one
//! part), and are only completed at the finish.
#![forbid(unsafe_code)]

use std::io;
use std::sync::Arc;
use std::time::SystemTime;

use rusty_s3::Method;

use super::{H_SUM, H_TOKEN, Inner, MPU_REF, PUT_REF, mtime_ms, normalise_etag, sum};
use crate::store::{Tag, Upload};

pub(super) struct S3Upload {
    inner: Arc<Inner>,
    key: String,
    size: u64,
    mtime_ms: Option<String>,
    /// Marks the object as this upload's own ([`sum::new_token`]).
    token: String,
    /// `None` for a single PUT.
    upload_id: Option<String>,
    /// ETag of each part sent so far, as the server gave it.
    parts: Vec<String>,
    buf: Vec<u8>,
    written: u64,
    sealed: bool,
}

/// How many bytes of `buffered` to send now as a non-final part: a full part, but only when
/// more than a part is held, so the last part is never empty.
pub(super) fn ready_part(buffered: usize, part_size: usize) -> Option<usize> {
    (buffered > part_size).then_some(part_size)
}

impl S3Upload {
    pub(super) fn begin(
        inner: Arc<Inner>,
        key: String,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> io::Result<Self> {
        let part = inner.limits.part_size as u64;
        let multipart = size > part;
        if multipart && size.div_ceil(part) > 10_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{key} is too big for one S3 upload"),
            ));
        }
        let mtime_ms = mtime.and_then(mtime_ms);
        let mut up = S3Upload {
            inner,
            key,
            size,
            mtime_ms,
            token: sum::new_token(),
            upload_id: None,
            parts: Vec::new(),
            buf: Vec::new(),
            written: 0,
            sealed: false,
        };
        if multipart {
            up.start_multipart()?;
        }
        Ok(up)
    }

    fn start_multipart(&mut self) -> io::Result<()> {
        let id =
            self.inner
                .create_multipart(&self.key, self.mtime_ms.as_deref(), Some(&self.token))?;
        self.upload_id = Some(id);
        Ok(())
    }

    fn send_part(&mut self, len: usize) -> io::Result<()> {
        let id = self.upload_id.clone().expect("multipart upload");
        let number = u16::try_from(self.parts.len() + 1).expect("part count is checked");
        let etag = self
            .inner
            .upload_part(&self.key, &id, number, &self.buf[..len])?;
        // Only drop the bytes once the part is safely there, so a failed call can be repeated.
        self.buf.drain(..len);
        self.parts.push(etag);
        Ok(())
    }

    fn drain_ready(&mut self) -> io::Result<()> {
        while let Some(len) = ready_part(self.buf.len(), self.inner.limits.part_size) {
            self.send_part(len)?;
        }
        Ok(())
    }

    fn abort_upload(&mut self) {
        if let Some(id) = self.upload_id.take() {
            let _ = self.inner.abort_multipart(&self.key, &id);
        }
    }
}

impl Upload for S3Upload {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if self.sealed {
            return Err(io::Error::other("the upload is sealed"));
        }
        if self.written + data.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "more bytes than the {}-byte size given for {}",
                    self.size, self.key
                ),
            ));
        }
        self.buf.extend_from_slice(data);
        self.written += data.len() as u64;
        if self.upload_id.is_some() {
            self.drain_ready()?;
        }
        Ok(())
    }

    fn seal(&mut self) -> io::Result<()> {
        if self.sealed {
            return Ok(());
        }
        if self.written != self.size {
            let msg = format!(
                "{} bytes were written for a {}-byte file ({})",
                self.written, self.size, self.key
            );
            self.abort_upload();
            return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
        }
        if self.upload_id.is_some() {
            self.drain_ready()?;
            if !self.buf.is_empty() {
                self.send_part(self.buf.len())?;
            }
        }
        self.sealed = true;
        Ok(())
    }

    fn stage(&mut self) -> io::Result<()> {
        self.seal()?;
        // A small file is still only in memory: send it as the one part of an upload. (A bigger
        // one has all its parts up already.) If a step fails the upload stays for `abort`, and
        // calling this again carries on from where it stopped.
        if self.upload_id.is_none() {
            self.start_multipart()?;
        }
        if self.parts.is_empty() {
            self.send_part(self.buf.len())?;
        }
        Ok(())
    }

    fn finish_no_replace(mut self: Box<Self>) -> io::Result<Tag> {
        self.seal()?;
        let inner = self.inner.clone();
        let c = &inner.client;
        let key = self.key.clone();
        match self.upload_id.clone() {
            None => {
                let ms = self.mtime_ms.clone();
                let sum = inner.sum_header(&self.buf);
                let mut hdrs: Vec<(&str, &str)> =
                    vec![("if-none-match", "*"), (H_TOKEN, &self.token)];
                hdrs.extend(sum.as_deref().map(|s| (H_SUM, s)));
                if let Some(ms) = &ms {
                    hdrs.push(("x-amz-meta-pnk-mtime", ms));
                }
                let r = c.exec(Method::Put, &hdrs, &self.buf, |h| {
                    super::http::Client::presign(c.bucket.put_object(Some(&c.creds), &key), h)
                })?;
                if r.status == 412 {
                    // An earlier try of this very write may have gone through.
                    if r.retried
                        && let Some(tag) = inner.own_write(&key, &self.token, self.size)
                    {
                        return Ok(tag);
                    }
                    return Err(super::already_exists(&key));
                }
                if !r.ok() {
                    return Err(c.fail(&r, &format!("write {key}")));
                }
                match r.header("etag") {
                    Some(t) => Ok(normalise_etag(t)),
                    None => inner
                        .head(&key)?
                        .and_then(|h| h.etag)
                        .ok_or_else(|| io::Error::other(format!("{key} vanished after its write"))),
                }
            }
            Some(id) => {
                let (token, size) = (self.token.clone(), self.size);
                let ours =
                    move |h: &super::Head| h.token.as_deref() == Some(&token) && h.size == size;
                let done = inner.complete(&key, &id, &self.parts, Some(&ours));
                match done {
                    Ok(tag) => {
                        self.upload_id = None;
                        Ok(tag)
                    }
                    // Refused for the name: `complete` has dropped the upload.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        self.upload_id = None;
                        Err(e)
                    }
                    // Anything else leaves the upload for `abort_by_ref`.
                    Err(e) => Err(e),
                }
            }
        }
    }

    fn abort(mut self: Box<Self>) -> io::Result<()> {
        if let Some(id) = self.upload_id.take() {
            self.inner.abort_multipart(&self.key, &id)?;
        }
        Ok(())
    }

    fn journal_ref(&self) -> String {
        match &self.upload_id {
            Some(id) => format!("{MPU_REF}{id}:{}", self.key),
            None => format!("{PUT_REF}{}", self.key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_leave_a_last_one() {
        assert_eq!(ready_part(10, 16), None);
        assert_eq!(ready_part(16, 16), None);
        assert_eq!(ready_part(17, 16), Some(16));
        // 40 bytes in 16-byte parts: 16, 16, then 8 at the seal.
        let (mut held, mut sent) = (40usize, Vec::new());
        while let Some(n) = ready_part(held, 16) {
            held -= n;
            sent.push(n);
        }
        assert_eq!((sent, held), (vec![16, 16], 8));
    }
}
