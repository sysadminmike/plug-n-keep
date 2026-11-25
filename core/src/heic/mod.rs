//! JPG copies of HEIC photos (DESIGN.md section 7.6).
//!
//! [`make_jpeg`] turns one HEIC file into a JPG beside it. The pixels come from the operating
//! system's own decoder (WIC on Windows, `libheif` loaded at run time on Linux and FreeBSD);
//! no decoder is built into the app. Everything else is the same on every system: the EXIF
//! block and the ICC colour profile are read out of the HEIF container ([`container`]),
//! the picture is turned upright ([`pixels`]), and one JPEG encoder writes the result with
//! both embedded.
//!
//! [`made`] is the list of JPGs the app made, which tells them from the device's own.
//!
//! This module only converts and records. When to convert, and what happens to a made JPG when
//! its HEIC is deleted or changed, belong to the sync.
#![forbid(unsafe_code)]

pub mod container;
pub mod made;
mod pixels;

use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use log::{debug, warn};

use crate::paths::JPG_TEMP_SUFFIX;

/// The quality the app uses for JPG copies.
pub const QUALITY: u8 = 90;

/// Most pixels on one side that the file may claim, and most RGB bytes (1 GiB) its picture may
/// take, before any decoding is tried.
const MAX_SIDE: u32 = 65535;
const MAX_RGB_BYTES: u64 = 1 << 30;

/// A decoded picture, as a system decoder hands it over.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    /// 8-bit RGB, three bytes per pixel, rows top to bottom, `width * height * 3` bytes. Not
    /// converted to another colour space: the file's ICC profile still describes it.
    pub rgb: Vec<u8>,
    /// Whether the decoder already applied the file's rotation and mirror.
    pub upright: bool,
}

/// Why a system decoder could not give a picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The decoder isn't installed; the text says what to install.
    Missing(String),
    Damaged(String),
    Unsupported(String),
}

/// Why a JPG could not be made. The text is plain words for the file list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MakeError {
    /// The system has no HEIC decoder; the text says what to install.
    DecoderMissing(String),
    /// The HEIC can't be read: damaged, or not a HEIC.
    Damaged(String),
    /// A HEIC this app doesn't convert, such as an image sequence.
    Unsupported(String),
    /// A file of the JPG's name is already there. Nothing was changed.
    TargetExists,
    /// A file problem: the folder is read-only, the disk is full.
    Io(String),
}

impl fmt::Display for MakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MakeError::DecoderMissing(hint) => write!(f, "{hint}"),
            MakeError::Damaged(why) => write!(f, "the HEIC photo can't be read ({why})"),
            MakeError::Unsupported(why) => write!(f, "not converted: {why}"),
            MakeError::TargetExists => write!(f, "a file with the JPG's name is already there"),
            MakeError::Io(why) => write!(f, "the JPG could not be written ({why})"),
        }
    }
}

impl std::error::Error for MakeError {}

/// What was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MadeInfo {
    pub width: u32,
    pub height: u32,
    /// The HEIC's size, for the made list.
    pub heic_size: u64,
    /// The HEIC's modified time, which the JPG was given too.
    pub heic_modified: SystemTime,
    /// Whether the HEIC's EXIF block was kept in the JPG.
    pub kept_exif: bool,
    /// Whether the HEIC's ICC colour profile was embedded in the JPG.
    pub kept_icc: bool,
}

/// Whether HEIC photos can be decoded on this computer. If not, the text says what to install.
pub fn available() -> Result<(), String> {
    #[cfg(test)]
    if let Some(r) = fake_decoder::available() {
        return r;
    }
    crate::platform::heic_available()
}

/// The system decoder, or in a test the stand-in ([`fake_decoder`]).
fn decode(path: &Path) -> Result<Decoded, DecodeError> {
    #[cfg(test)]
    if let Some(r) = fake_decoder::decode(path) {
        return r;
    }
    crate::platform::heic_decode(path)
}

/// A stand-in for the system's decoder, for tests of the code that decides when to convert. It
/// is per thread; with none set the real decoder is used. The container is still parsed for
/// real, so the HEIC a test gives must be a well-formed one (the fixtures are).
#[cfg(test)]
pub(crate) mod fake_decoder {
    use super::{DecodeError, Decoded};
    use std::cell::RefCell;
    use std::path::Path;

    type Decode = Box<dyn Fn(&Path) -> Result<Decoded, DecodeError>>;

    thread_local! {
        static FAKE: RefCell<Option<Decode>> = const { RefCell::new(None) };
        static AVAILABLE: RefCell<Option<Result<(), String>>> = const { RefCell::new(None) };
    }

    /// Puts the stand-in in place until the guard is dropped.
    pub struct Guard;

    pub fn set(f: impl Fn(&Path) -> Result<Decoded, DecodeError> + 'static) -> Guard {
        FAKE.with(|c| *c.borrow_mut() = Some(Box::new(f)));
        AVAILABLE.with(|c| *c.borrow_mut() = Some(Ok(())));
        Guard
    }

    /// A small grey upright picture of the size the file says (`ispe`, turned for quarter
    /// turns), or 8 x 6 if it says none: what a working decoder would hand over.
    pub fn picture_for(path: &Path) -> Decoded {
        let (mut w, mut h) = (8u32, 6u32);
        if let Ok(mut f) = std::fs::File::open(path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if let Ok(p) = super::container::read_properties(&mut f, len)
                && let Some((sw, sh)) = p.stored_size
            {
                (w, h) = (sw, sh);
                if p.transforms
                    .iter()
                    .any(|t| matches!(t, super::container::Transform::Rotate(1 | 3)))
                {
                    (w, h) = (sh, sw);
                }
            }
        }
        Decoded {
            width: w,
            height: h,
            rgb: vec![120; (w * h * 3) as usize],
            upright: true,
        }
    }

    /// A decoder that makes `picture_for` of every file.
    pub fn working() -> Guard {
        set(|path| Ok(picture_for(path)))
    }

    /// Reports the decoder as missing, with this hint.
    pub fn missing(hint: &str) -> Guard {
        let hint = hint.to_string();
        let h2 = hint.clone();
        FAKE.with(|c| {
            *c.borrow_mut() = Some(Box::new(move |_| Err(DecodeError::Missing(h2.clone()))))
        });
        AVAILABLE.with(|c| *c.borrow_mut() = Some(Err(hint)));
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FAKE.with(|c| *c.borrow_mut() = None);
            AVAILABLE.with(|c| *c.borrow_mut() = None);
        }
    }

    pub(super) fn decode(path: &Path) -> Option<Result<Decoded, DecodeError>> {
        FAKE.with(|c| c.borrow().as_ref().map(|f| f(path)))
    }

    pub(super) fn available() -> Option<Result<(), String>> {
        AVAILABLE.with(|c| c.borrow().clone())
    }
}

/// Whether a file name is a HEIC or HEIF photo: `.heic` or `.heif`, in any case.
pub fn is_heic_name(name: &str) -> bool {
    extension(name)
        .is_some_and(|e| e.eq_ignore_ascii_case("heic") || e.eq_ignore_ascii_case("heif"))
}

fn extension(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then_some(ext)
}

/// The name of the JPG made from the HEIC called `name`: the same name with `.jpg`, written in
/// the same case as the HEIC's extension (`.HEIC` gives `.JPG`, `.heic` gives `.jpg`, `.Heic`
/// gives `.Jpg`). `None` if `name` isn't a HEIC.
pub fn jpg_name_for(name: &str) -> Option<String> {
    if !is_heic_name(name) {
        return None;
    }
    let ext = extension(name)?;
    let stem = &name[..name.len() - ext.len()];
    let jpg = if ext.chars().all(|c| !c.is_ascii_lowercase()) {
        "JPG"
    } else if ext.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        "Jpg"
    } else {
        "jpg"
    };
    Some(format!("{stem}{jpg}"))
}

/// Whether a file name is a JPEG: `.jpg` or `.jpeg`, in any case.
pub fn is_jpeg_name(name: &str) -> bool {
    extension(name).is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

/// Makes the JPG `jpg` from the HEIC `heic`. The JPG is written to a temporary file in the
/// same folder and moved into place without replacing anything, so an existing file of that
/// name is an error and is never touched. The JPG gets the HEIC's modified time.
pub fn make_jpeg(heic: &Path, jpg: &Path, quality: u8) -> Result<MadeInfo, MakeError> {
    let io_err = |e: std::io::Error| MakeError::Io(e.to_string());
    if fs::symlink_metadata(jpg).is_ok() {
        return Err(MakeError::TargetExists);
    }
    let meta = fs::metadata(heic).map_err(|e| MakeError::Damaged(e.to_string()))?;
    if !meta.is_file() {
        return Err(MakeError::Damaged("it is not a file".into()));
    }
    let modified = meta.modified().map_err(io_err)?;

    let props = {
        let mut f = File::open(heic).map_err(|e| MakeError::Damaged(e.to_string()))?;
        container::read_properties(&mut f, meta.len()).map_err(|e| match e {
            container::ParseError::Damaged(w) => MakeError::Damaged(w),
            container::ParseError::Unsupported(w) => MakeError::Unsupported(w),
        })?
    };

    // Refuse a hostile size before any pixel is allocated.
    if let Some((sw, sh)) = props.stored_size
        && (sw > MAX_SIDE || sh > MAX_SIDE || u64::from(sw) * u64::from(sh) * 3 > MAX_RGB_BYTES)
    {
        return Err(MakeError::Unsupported(
            "the photo is too large to convert".into(),
        ));
    }
    let decoded = decode(heic).map_err(|e| match e {
        DecodeError::Missing(hint) => MakeError::DecoderMissing(hint),
        DecodeError::Damaged(w) => MakeError::Damaged(w),
        DecodeError::Unsupported(w) => MakeError::Unsupported(w),
    })?;
    let expected = (decoded.width as usize)
        .checked_mul(decoded.height as usize)
        .and_then(|n| n.checked_mul(3));
    if decoded.width == 0 || decoded.height == 0 || expected != Some(decoded.rgb.len()) {
        return Err(MakeError::Damaged(
            "the decoder gave a broken picture".into(),
        ));
    }
    if let Some((sw, sh)) = props.stored_size {
        let quarter_turn = props
            .transforms
            .iter()
            .any(|t| matches!(t, container::Transform::Rotate(1 | 3)));
        let got = (decoded.width, decoded.height);
        // A decoder that applied the rotation gives the turned size; accept either way round
        // for those, since which one they report is theirs to decide.
        let fits = if decoded.upright && quarter_turn {
            got == (sh, sw) || got == (sw, sh)
        } else {
            got == (sw, sh)
        };
        if !fits {
            return Err(MakeError::Damaged(
                "the decoder gave a picture of a different size than the file says".into(),
            ));
        }
    }
    let mut image = pixels::Rgb {
        width: decoded.width,
        height: decoded.height,
        data: decoded.rgb,
    };
    if !decoded.upright {
        image = image.apply(&props.transforms);
    }
    let (Ok(w), Ok(h)) = (u16::try_from(image.width), u16::try_from(image.height)) else {
        return Err(MakeError::Unsupported(
            "the photo is too large for a JPG (over 65535 pixels on a side)".into(),
        ));
    };

    let mut bytes = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut bytes, quality.clamp(1, 100));
    let mut kept_icc = false;
    if let Some(icc) = &props.icc {
        match enc.add_icc_profile(icc) {
            Ok(()) => kept_icc = true,
            Err(e) => warn!("The colour profile of {} was left out: {e}", heic.display()),
        }
    }
    let mut kept_exif = false;
    if let Some(exif) = &props.exif {
        // The picture is upright now, so the tag must say so.
        let mut exif = exif.clone();
        pixels::set_upright(&mut exif);
        match enc.add_exif_metadata(&exif) {
            Ok(()) => kept_exif = true,
            Err(e) => warn!("The EXIF data of {} was left out: {e}", heic.display()),
        }
    }
    enc.encode(&image.data, w, h, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| MakeError::Io(format!("encoding failed: {e}")))?;
    drop(image);

    let name = jpg
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| MakeError::Io("no file name".into()))?;
    // A suffix of its own, which the copy step never produces (its temp is a device file's name
    // plus `.pnk-part`) and which no device file may use, so a stale one left by a crash is
    // ours to remove, never a device file's copy in progress.
    let temp = jpg.with_file_name(crate::paths::fit_name(&name, JPG_TEMP_SUFFIX));
    let _ = fs::remove_file(&temp);
    let written = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        f.write_all(&bytes)?;
        crate::platform::flush_file(&f)?;
        crate::platform::set_modified(&f, modified)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(io_err(e));
    }
    if let Err(e) = crate::platform::rename_no_replace(&temp, jpg) {
        let _ = fs::remove_file(&temp);
        return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
            MakeError::TargetExists
        } else {
            io_err(e)
        });
    }
    debug!("Made {} from {}", jpg.display(), heic.display());
    Ok(MadeInfo {
        width: u32::from(w),
        height: u32::from(h),
        heic_size: meta.len(),
        heic_modified: modified,
        kept_exif,
        kept_icc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_case_of_the_original() {
        assert_eq!(
            jpg_name_for("IMG_0001.HEIC").as_deref(),
            Some("IMG_0001.JPG")
        );
        assert_eq!(
            jpg_name_for("img_0001.heic").as_deref(),
            Some("img_0001.jpg")
        );
        assert_eq!(jpg_name_for("IMG.HEIF").as_deref(), Some("IMG.JPG"));
        assert_eq!(jpg_name_for("a.b.Heic").as_deref(), Some("a.b.Jpg"));
        assert_eq!(jpg_name_for("IMG.hEIC").as_deref(), Some("IMG.jpg"));
        assert_eq!(jpg_name_for("IMG.jpg"), None);
        assert_eq!(jpg_name_for("heic"), None);
        assert_eq!(jpg_name_for(".heic"), None);
        assert_eq!(jpg_name_for("IMG.heic.mov"), None);
    }

    #[test]
    fn heic_and_jpeg_names() {
        for n in ["a.heic", "a.HEIF", "a.Heif"] {
            assert!(is_heic_name(n), "{n}");
        }
        for n in ["a.hei", "a.heics", "a", "a.jpg"] {
            assert!(!is_heic_name(n), "{n}");
        }
        for n in ["a.jpg", "a.JPEG", "a.Jpeg", "a.JPG"] {
            assert!(is_jpeg_name(n), "{n}");
        }
        assert!(!is_jpeg_name("a.jpe") && !is_jpeg_name("a.png"));
    }

    #[test]
    fn an_existing_target_is_never_touched() {
        let t = tempfile::tempdir().unwrap();
        let heic = t.path().join("a.HEIC");
        let jpg = t.path().join("a.JPG");
        fs::write(&heic, b"x").unwrap();
        fs::write(&jpg, b"the device's own").unwrap();
        assert_eq!(make_jpeg(&heic, &jpg, 90), Err(MakeError::TargetExists));
        assert_eq!(fs::read(&jpg).unwrap(), b"the device's own");
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_file_that_is_not_a_heic_is_damaged_and_leaves_nothing_behind() {
        let t = tempfile::tempdir().unwrap();
        let heic = t.path().join("a.HEIC");
        let jpg = t.path().join("a.JPG");
        fs::write(&heic, b"this is not a photo at all").unwrap();
        assert!(matches!(
            make_jpeg(&heic, &jpg, 90),
            Err(MakeError::Damaged(_))
        ));
        assert!(!jpg.exists());
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
        assert!(matches!(
            make_jpeg(&t.path().join("none.HEIC"), &jpg, 90),
            Err(MakeError::Damaged(_))
        ));
    }

    #[test]
    fn a_huge_claimed_size_is_refused_before_decoding_and_a_wrong_decoded_size_after() {
        use container::tests::{full, heif, tiff_le};
        let t = tempfile::tempdir().unwrap();
        let heic = t.path().join("a.HEIC");
        let jpg = t.path().join("a.JPG");
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let c2 = calls.clone();
        let _g = fake_decoder::set(move |_| {
            c2.set(c2.get() + 1);
            Ok(Decoded {
                width: 8,
                height: 6,
                rgb: vec![1; 8 * 6 * 3],
                upright: true,
            })
        });
        let huge = full(b"ispe", 0, 0, &[0, 0, 0xC0, 0, 0, 0, 0xC0, 0]);
        fs::write(&heic, heif(&tiff_le(1), &[huge], &[1])).unwrap();
        assert!(matches!(
            make_jpeg(&heic, &jpg, 90),
            Err(MakeError::Unsupported(_))
        ));
        let wide = full(b"ispe", 0, 0, &[0, 1, 0, 0, 0, 0, 0, 1]);
        fs::write(&heic, heif(&tiff_le(1), &[wide], &[1])).unwrap();
        assert!(matches!(
            make_jpeg(&heic, &jpg, 90),
            Err(MakeError::Unsupported(_))
        ));
        assert_eq!(calls.get(), 0, "never decoded");
        let other = full(b"ispe", 0, 0, &[0, 0, 0, 20, 0, 0, 0, 20]);
        fs::write(&heic, heif(&tiff_le(1), &[other], &[1])).unwrap();
        assert!(matches!(
            make_jpeg(&heic, &jpg, 90),
            Err(MakeError::Damaged(_))
        ));
        assert_eq!(calls.get(), 1);
        assert!(!jpg.exists());
        let right = full(b"ispe", 0, 0, &[0, 0, 0, 8, 0, 0, 0, 6]);
        fs::write(&heic, heif(&tiff_le(1), &[right], &[1])).unwrap();
        assert!(make_jpeg(&heic, &jpg, 90).is_ok());
    }

    #[test]
    fn errors_are_plain_words() {
        for e in [
            MakeError::DecoderMissing("install the thing".into()),
            MakeError::Damaged("x".into()),
            MakeError::Unsupported("y".into()),
            MakeError::TargetExists,
            MakeError::Io("z".into()),
        ] {
            let s = e.to_string();
            assert!(!s.is_empty() && !s.contains("Error"), "{s}");
        }
    }

    /// Converts every HEIC in the folder named by `PNK_HEIC_SAMPLES` into the folder named by
    /// `PNK_HEIC_OUT`, and prints what came out. For looking at the results by hand; real
    /// photos are never kept in the repository.
    #[test]
    #[ignore = "needs real HEIC photos and a decoder: set PNK_HEIC_SAMPLES and PNK_HEIC_OUT"]
    fn converts_the_real_samples() {
        let (Some(from), Some(to)) = (
            std::env::var_os("PNK_HEIC_SAMPLES"),
            std::env::var_os("PNK_HEIC_OUT"),
        ) else {
            panic!("set PNK_HEIC_SAMPLES and PNK_HEIC_OUT");
        };
        fs::create_dir_all(&to).unwrap();
        let mut count = 0;
        for e in fs::read_dir(from).unwrap() {
            let heic = e.unwrap().path();
            let name = heic.file_name().unwrap().to_string_lossy().into_owned();
            let Some(jpg_name) = jpg_name_for(&name) else {
                continue;
            };
            let props = container::read_properties(
                &mut File::open(&heic).unwrap(),
                fs::metadata(&heic).unwrap().len(),
            )
            .unwrap();
            let jpg = Path::new(&to).join(jpg_name);
            let _ = fs::remove_file(&jpg);
            let info = make_jpeg(&heic, &jpg, QUALITY).unwrap();
            println!(
                "{name}: stored {:?}, transforms {:?}, exif {} bytes (orientation {:?}), icc {} bytes -> {}x{}, kept exif {} icc {}, {} bytes",
                props.stored_size,
                props.transforms,
                props.exif.as_ref().map_or(0, Vec::len),
                props.exif.as_deref().and_then(pixels::orientation),
                props.icc.as_ref().map_or(0, Vec::len),
                info.width,
                info.height,
                info.kept_exif,
                info.kept_icc,
                fs::metadata(&jpg).unwrap().len()
            );
            assert_eq!(
                fs::metadata(&jpg).unwrap().modified().unwrap(),
                info.heic_modified
            );
            count += 1;
        }
        assert!(count > 0);
    }

    // ---- real conversions, on fixtures made with the Windows HEIF encoder (see fixtures/) ----

    const PLAIN: &[u8] = include_bytes!("fixtures/plain.heic");
    const ROTATED: &[u8] = include_bytes!("fixtures/rotated.heic");
    const MIRRORED: &[u8] = include_bytes!("fixtures/mirrored.heic");
    const WITH_ICC: &[u8] = include_bytes!("fixtures/icc.heic");
    /// Like `rotated.heic`, with an `auxl` reference to an item that isn't in the file, as some
    /// iPhone photos have. `libheif` 1.17 refuses those unless the reference is retired first.
    const DANGLING: &[u8] = include_bytes!("fixtures/dangling.heic");

    /// The fixture picture is 64 x 48: a magenta top-left, a blue top-right, a red bottom-left
    /// and a green bottom-right corner.
    fn colour_of(px: [u8; 3]) -> &'static str {
        let [r, g, b] = px.map(i32::from);
        match (r > 150, g > 120, b > 150) {
            (true, false, true) => "magenta",
            (false, false, true) => "blue",
            (true, _, false) => "red",
            (false, true, false) => "green",
            _ => "other",
        }
    }

    struct Made {
        info: MadeInfo,
        bytes: Vec<u8>,
        dims: (u16, u16),
        corners: [&'static str; 4],
        heic_modified: SystemTime,
        jpg_modified: SystemTime,
    }

    /// Makes a JPG from `heic_bytes` and reads it back; `None` when this computer can't decode
    /// HEIC (the tests then have nothing to check).
    fn convert(heic_bytes: &[u8]) -> Option<Made> {
        if let Err(hint) = available() {
            eprintln!("skipped: {hint}");
            return None;
        }
        let t = tempfile::tempdir().unwrap();
        let heic = t.path().join("P.HEIC");
        let jpg = t.path().join("P.JPG");
        fs::write(&heic, heic_bytes).unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(40 * 86400);
        let f = fs::OpenOptions::new().write(true).open(&heic).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let info = make_jpeg(&heic, &jpg, QUALITY).unwrap();
        assert_eq!(
            fs::read_dir(t.path()).unwrap().count(),
            2,
            "no temp file left"
        );
        let bytes = fs::read(&jpg).unwrap();
        let mut dec = jpeg_decoder::Decoder::new(&bytes[..]);
        let px = dec.decode().unwrap();
        let i = dec.info().unwrap();
        let (w, h) = (usize::from(i.width), usize::from(i.height));
        let at = |x: usize, y: usize| {
            let o = (y * w + x) * 3;
            [px[o], px[o + 1], px[o + 2]]
        };
        let corners = [
            colour_of(at(6, 3)),
            colour_of(at(w - 7, 3)),
            colour_of(at(6, h - 4)),
            colour_of(at(w - 7, h - 4)),
        ];
        Some(Made {
            info,
            bytes,
            dims: (i.width, i.height),
            corners,
            heic_modified: fs::metadata(&heic).unwrap().modified().unwrap(),
            jpg_modified: fs::metadata(&jpg).unwrap().modified().unwrap(),
        })
    }

    /// The EXIF block of a JPG (after `Exif  `), if it has one.
    fn exif_of(jpg: &[u8]) -> Option<Vec<u8>> {
        let i = jpg.windows(6).position(|w| w == b"Exif  ")?;
        let len = usize::from(u16::from_be_bytes([jpg[i - 4 + 2], jpg[i - 4 + 3]]));
        Some(jpg[i + 6..i - 4 + 2 + len].to_vec())
    }

    #[test]
    fn a_plain_landscape_photo_converts() {
        let Some(m) = convert(PLAIN) else { return };
        assert_eq!(m.dims, (64, 48));
        assert_eq!(m.corners, ["magenta", "blue", "red", "green"]);
        assert!(!m.info.kept_exif && !m.info.kept_icc);
        assert_eq!(
            m.jpg_modified, m.heic_modified,
            "the JPG takes the HEIC's date"
        );
        assert_eq!(m.info.heic_modified, m.heic_modified);
        assert!(m.bytes.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn a_rotated_photo_comes_out_upright_with_orientation_one() {
        let Some(m) = convert(ROTATED) else { return };
        assert_eq!(m.dims, (48, 64), "turned a quarter turn");
        assert_eq!(m.corners, ["blue", "green", "magenta", "red"]);
        assert!(m.info.kept_exif);
        let exif = exif_of(&m.bytes).unwrap();
        assert_eq!(pixels::orientation(&exif), Some(1), "the source said 6");
        assert!(
            exif.windows(3).any(|w| w == b"PNK"),
            "the rest of the EXIF is kept"
        );
    }

    #[test]
    fn a_photo_with_a_reference_to_a_missing_item_converts() {
        let Some(m) = convert(DANGLING) else { return };
        assert_eq!(m.dims, (48, 64), "turned a quarter turn");
        assert_eq!(m.corners, ["blue", "green", "magenta", "red"]);
        assert!(m.info.kept_exif);
    }

    #[test]
    fn a_mirrored_photo_comes_out_the_right_way_round() {
        let Some(m) = convert(MIRRORED) else { return };
        assert_eq!(m.dims, (64, 48));
        assert_eq!(
            m.corners,
            ["red", "green", "magenta", "blue"],
            "top and bottom swapped"
        );
    }

    #[test]
    fn the_colour_profile_and_exif_are_embedded() {
        let Some(m) = convert(WITH_ICC) else { return };
        assert!(m.info.kept_icc && m.info.kept_exif);
        let profile =
            container::read_properties(&mut std::io::Cursor::new(WITH_ICC), WITH_ICC.len() as u64)
                .unwrap()
                .icc
                .unwrap();
        let at = m
            .bytes
            .windows(12)
            .position(|w| w == b"ICC_PROFILE ")
            .unwrap();
        assert!(
            m.bytes[at + 14..].starts_with(&profile),
            "the profile is embedded as it was"
        );
        assert_eq!(pixels::orientation(&exif_of(&m.bytes).unwrap()), Some(1));
    }
}
