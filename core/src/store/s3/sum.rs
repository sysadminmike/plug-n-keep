//! The checksum sent with every upload, and the token that marks a write as this app's own.
#![forbid(unsafe_code)]

use sha2::{Digest, Sha256};

/// `x-amz-checksum-sha256` for `data`: the SHA-256 in standard base64.
pub(super) fn sha256_b64(data: &[u8]) -> String {
    base64(Sha256::digest(data).as_ref())
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A fresh random token. Every object this app writes carries one (`x-amz-meta-pnk-token`), so
/// after a request that was sent again, "it is already there" can be told from "someone else
/// put it there": only our own write has our token.
pub(super) fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_cases() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn sha256_is_what_the_server_computes() {
        // Checked against what the test server returned for these bytes.
        assert_eq!(
            sha256_b64(
                b"hello
"
            ),
            "WJG1tSLV3whtD/CxEPvZ0hu0/HFjrzTQgoai6Eb2vgM="
        );
    }

    #[test]
    fn tokens_differ() {
        assert_ne!(new_token(), new_token());
    }
}
