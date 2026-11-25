//! Decoding a HEIC or HEIF photo with `libheif`, loaded at run time (see `dynlib.rs`). Nothing
//! is bundled into the app: without the library, or without its HEVC decoder plugin, JPG copies
//! of HEIC photos are off and the hint says what to install. DESIGN.md section 7.6.
//!
//! Only reading functions are declared: nothing that encodes or writes a HEIF file. `libheif`
//! applies the file's rotation and mirror itself, so the picture it returns is upright.
//!
//! **The file is read into memory first.** `libheif` 1.17 (Ubuntu 24.04) fails some iPhone photos
//! with "Non-existing item ID referenced", because an `iref` reference names an item that isn't
//! there. [`crate::heic::container::retire_dangling_references`] makes such references harmless in
//! the bytes, and `libheif` reads the cleaned bytes ([`ffi::heif_context_read_from_memory_without_copy`]).
//! The photo on disk is never touched.

use std::ffi::{CStr, c_int, c_void};
use std::path::Path;
use std::ptr;

use crate::heic::{DecodeError, Decoded};

/// The biggest HEIC file read into memory for decoding. Phone photos are a few MB.
const MAX_FILE: u64 = 256 * 1024 * 1024;

const HINT: &str = "JPG copies of HEIC photos need libheif \
                    (Ubuntu: sudo apt install libheif1 libheif-plugin-libde265)";

mod ffi {
    use std::ffi::{c_char, c_int, c_void};

    /// `struct heif_error`, returned by value. `code` 0 is "no error".
    #[repr(C)]
    pub struct HeifError {
        pub code: c_int,
        pub subcode: c_int,
        pub message: *const c_char,
    }

    /// `heif_colorspace_RGB`.
    pub const COLORSPACE_RGB: c_int = 1;
    /// `heif_chroma_interleaved_RGB`: 8 bits per channel, R G B, one plane.
    pub const CHROMA_INTERLEAVED_RGB: c_int = 10;
    /// `heif_channel_interleaved`.
    pub const CHANNEL_INTERLEAVED: c_int = 10;
    /// `heif_compression_HEVC`.
    pub const COMPRESSION_HEVC: c_int = 1;

    super::super::dynlib::dyn_api! {
        sonames: &["libheif.so.1", "libheif.so"],
        off: "JPG copies of HEIC photos are off: libheif isn't installed",
        api: HeifApi,
        get: heif_api,
        load: load_heif_api;
        pub fn heif_context_alloc() -> *mut c_void;
        pub fn heif_context_free(ctx: *mut c_void);
        pub fn heif_context_read_from_memory_without_copy(
            ctx: *mut c_void,
            mem: *const c_void,
            size: usize,
            options: *const c_void,
        ) -> HeifError;
        pub fn heif_context_get_primary_image_handle(
            ctx: *mut c_void,
            handle: *mut *mut c_void,
        ) -> HeifError;
        pub fn heif_image_handle_release(handle: *const c_void);
        pub fn heif_decode_image(
            handle: *const c_void,
            image: *mut *mut c_void,
            colorspace: c_int,
            chroma: c_int,
            options: *const c_void,
        ) -> HeifError;
        pub fn heif_image_get_plane_readonly(
            image: *const c_void,
            channel: c_int,
            stride: *mut c_int,
        ) -> *const u8;
        pub fn heif_image_get_width(image: *const c_void, channel: c_int) -> c_int;
        pub fn heif_image_get_height(image: *const c_void, channel: c_int) -> c_int;
        pub fn heif_image_release(image: *const c_void);
        pub fn heif_have_decoder_for_format(format: c_int) -> c_int;
    }

    pub fn available() -> bool {
        heif_api().is_some()
    }
}

/// Whether HEIC photos can be decoded: `libheif` is there and has a decoder for HEVC.
pub fn available() -> Result<(), String> {
    if !ffi::available() {
        return Err(HINT.into());
    }
    // SAFETY: the library was checked above; the function takes a plain number.
    if unsafe { ffi::heif_have_decoder_for_format(ffi::COMPRESSION_HEVC) } == 0 {
        return Err(HINT.into());
    }
    Ok(())
}

/// A `libheif` object released when dropped.
struct Owned {
    ptr: *mut c_void,
    release: unsafe fn(*mut c_void),
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr` came from the matching `libheif` call and is released only here.
            unsafe { (self.release)(self.ptr) };
        }
    }
}

unsafe fn free_context(p: *mut c_void) {
    // SAFETY: forwarded: `p` is a context from `heif_context_alloc`.
    unsafe { ffi::heif_context_free(p) }
}
unsafe fn release_handle(p: *mut c_void) {
    // SAFETY: forwarded: `p` is an image handle.
    unsafe { ffi::heif_image_handle_release(p) }
}
unsafe fn release_image(p: *mut c_void) {
    // SAFETY: forwarded: `p` is a decoded image.
    unsafe { ffi::heif_image_release(p) }
}

/// Turns a `libheif` error into ours.
fn error(e: &ffi::HeifError, what: &str) -> DecodeError {
    let message = if e.message.is_null() {
        String::new()
    } else {
        // SAFETY: `libheif` error messages are static NUL-ended strings.
        unsafe { CStr::from_ptr(e.message) }
            .to_string_lossy()
            .into_owned()
    };
    // Codes: 3 unsupported file type, 4 unsupported feature (sub-code 4003: unsupported codec),
    // 11 plugin loading.
    match (e.code, e.subcode) {
        (4, 4003) | (11, _) => DecodeError::Missing(HINT.into()),
        (3, _) | (4, _) => DecodeError::Unsupported(format!("{what}: {message}")),
        _ => DecodeError::Damaged(format!("{what}: {message}")),
    }
}

/// Decodes the primary image to 8-bit RGB. `libheif` applies the rotation and mirror.
pub fn decode(path: &Path) -> Result<Decoded, DecodeError> {
    available().map_err(DecodeError::Missing)?;
    let io = |e: std::io::Error| DecodeError::Damaged(format!("the photo could not be read: {e}"));
    if std::fs::metadata(path).map_err(io)?.len() > MAX_FILE {
        return Err(DecodeError::Unsupported(
            "the photo is too large to convert".into(),
        ));
    }
    // Declared before the context, so it is dropped after it: the context reads from it.
    let mut bytes = std::fs::read(path).map_err(io)?;
    let retired = crate::heic::container::retire_dangling_references(&mut bytes);
    if retired > 0 {
        log::debug!(
            "{}: {retired} reference(s) to items that aren't in the file were ignored",
            path.display()
        );
    }
    // SAFETY (whole block): the library was checked above; every pointer passed is a live
    // object from the preceding call, objects are released by `Owned` on every path (before
    // `bytes`, which the context reads and so outlives it), and the pixel rows read are inside
    // the plane `libheif` reports (stride and height are checked).
    unsafe {
        let ctx = Owned {
            ptr: ffi::heif_context_alloc(),
            release: free_context,
        };
        if ctx.ptr.is_null() {
            return Err(DecodeError::Damaged("out of memory".into()));
        }
        let e = ffi::heif_context_read_from_memory_without_copy(
            ctx.ptr,
            bytes.as_ptr().cast(),
            bytes.len(),
            ptr::null(),
        );
        if e.code != 0 {
            return Err(error(&e, "the photo could not be opened"));
        }
        let mut handle = Owned {
            ptr: ptr::null_mut(),
            release: release_handle,
        };
        let e = ffi::heif_context_get_primary_image_handle(ctx.ptr, &mut handle.ptr);
        if e.code != 0 {
            return Err(error(&e, "the photo has no main image"));
        }
        let mut image = Owned {
            ptr: ptr::null_mut(),
            release: release_image,
        };
        let e = ffi::heif_decode_image(
            handle.ptr,
            &mut image.ptr,
            ffi::COLORSPACE_RGB,
            ffi::CHROMA_INTERLEAVED_RGB,
            ptr::null(),
        );
        if e.code != 0 {
            return Err(error(&e, "the photo could not be decoded"));
        }
        let w = ffi::heif_image_get_width(image.ptr, ffi::CHANNEL_INTERLEAVED);
        let h = ffi::heif_image_get_height(image.ptr, ffi::CHANNEL_INTERLEAVED);
        let mut stride: c_int = 0;
        let plane =
            ffi::heif_image_get_plane_readonly(image.ptr, ffi::CHANNEL_INTERLEAVED, &mut stride);
        let (Ok(w), Ok(h), Ok(stride)) = (
            usize::try_from(w),
            usize::try_from(h),
            usize::try_from(stride),
        ) else {
            return Err(DecodeError::Damaged(
                "the decoder gave a broken size".into(),
            ));
        };
        let row = w
            .checked_mul(3)
            .filter(|row| w > 0 && h > 0 && stride >= *row && !plane.is_null())
            .ok_or_else(|| DecodeError::Damaged("the decoder gave a broken picture".into()))?;
        let total = row
            .checked_mul(h)
            .filter(|t| *t <= 1 << 30)
            .ok_or_else(|| DecodeError::Unsupported("the photo's size is not supported".into()))?;
        let mut rgb = Vec::with_capacity(total);
        for y in 0..h {
            rgb.extend_from_slice(std::slice::from_raw_parts(plane.add(y * stride), row));
        }
        Ok(Decoded {
            width: w as u32,
            height: h as u32,
            rgb,
            upright: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only reading functions are declared, and none that writes a HEIF file.
    #[test]
    fn only_reading_functions_are_declared() {
        const ALLOWED: [&str; 11] = [
            "heif_context_alloc",
            "heif_context_free",
            "heif_context_read_from_memory_without_copy",
            "heif_context_get_primary_image_handle",
            "heif_image_handle_release",
            "heif_decode_image",
            "heif_image_get_plane_readonly",
            "heif_image_get_width",
            "heif_image_get_height",
            "heif_image_release",
            "heif_have_decoder_for_format",
        ];
        let prod = super::super::dynlib::check::production(include_str!("heic.rs"));
        let declared = super::super::dynlib::check::declared(&prod);
        assert_eq!(declared.len(), ALLOWED.len(), "{declared:?}");
        for name in &declared {
            assert!(ALLOWED.contains(&name.as_str()), "{name} is not allowed");
        }
        for bad in [
            "heif_context_write",
            "heif_context_add",
            "heif_context_new",
            "heif_encoder",
            "heif_image_create",
            "heif_context_encode",
        ] {
            assert!(!prod.contains(bad), "{bad} must not appear");
        }
        for direct in ["dlsym", "dlopen", ".sym(", "Lib::open", "extern \"C\" {"] {
            assert!(!prod.contains(direct), "{direct} belongs in dynlib.rs only");
        }
    }

    /// Without the library (or with the test hook set) the answer is the hint, not a crash.
    #[test]
    fn without_the_library_the_hint_says_what_to_install() {
        match available() {
            Ok(()) => {}
            Err(hint) => {
                assert!(
                    hint.contains("libheif") && hint.contains("apt install"),
                    "{hint}"
                );
                let t = tempfile::tempdir().unwrap();
                let f = t.path().join("a.heic");
                std::fs::write(&f, b"x").unwrap();
                assert!(matches!(decode(&f), Err(DecodeError::Missing(_))));
            }
        }
    }

    /// A file that is not a HEIC fails cleanly when the library is there.
    #[test]
    fn a_file_that_is_not_a_heic_is_not_a_crash() {
        if available().is_err() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("a.heic");
        std::fs::write(
            &f,
            b"this is not a photo, just text, long enough to look at",
        )
        .unwrap();
        assert!(decode(&f).is_err());
    }
}
