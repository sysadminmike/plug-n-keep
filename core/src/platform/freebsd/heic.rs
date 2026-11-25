//! HEIC photos on FreeBSD: no system decoder is used yet, so JPG copies of HEIC photos are
//! unavailable here. (The Linux module loads `libheif` at run time; FreeBSD's build does not
//! have that loader yet.)

use std::path::Path;

use crate::heic::{DecodeError, Decoded};

const HINT: &str = "JPG copies of HEIC photos are not available on FreeBSD yet";

pub fn available() -> Result<(), String> {
    Err(HINT.into())
}

pub fn decode(_path: &Path) -> Result<Decoded, DecodeError> {
    Err(DecodeError::Missing(HINT.into()))
}
