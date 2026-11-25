//! Decoding a HEIC or HEIF photo with the Windows Imaging Component (WIC), which uses the
//! decoders Windows (or the Store's "HEIF Image Extensions" and "HEVC Video Extensions") has
//! installed. Nothing is bundled into the app. See DESIGN.md section 7.6.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{GENERIC_READ, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatHeif, GUID_WICPixelFormat24bppRGB,
    IWICBitmapDecoderInfo, IWICImagingFactory, WICComponentEnumerateDefault,
    WICConvertBitmapSource, WICDecodeMetadataCacheOnDemand, WICDecoder,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    IEnumUnknown,
};
use windows::core::{HRESULT, PCWSTR};

use crate::heic::{DecodeError, Decoded};

const HINT: &str = "JPG copies of HEIC photos need Windows' \"HEIF Image Extensions\" and \
                    \"HEVC Video Extensions\" (free in the Microsoft Store)";

// WINCODEC_ERR_* codes (winerror.h) that mean "no decoder for this".
const COMPONENT_NOT_FOUND: HRESULT = HRESULT(0x8898_2F50_u32 as i32);
const UNKNOWN_IMAGE_FORMAT: HRESULT = HRESULT(0x8898_2F07_u32 as i32);
const E_NOT_IMPL: HRESULT = HRESULT(0x8000_4001_u32 as i32);

/// COM for the current thread, undone when dropped.
struct Com(bool);

impl Com {
    fn start() -> Result<Com, String> {
        // SAFETY: plain per-thread COM setup; undone in `drop` when it succeeded.
        let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if init.is_ok() {
            Ok(Com(true))
        } else if init == RPC_E_CHANGED_MODE {
            // The thread already has COM in another mode: use it as it is.
            Ok(Com(false))
        } else {
            Err(format!(
                "could not start Windows' image component: {}",
                windows::core::Error::from(init)
            ))
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: pairs with the successful CoInitializeEx in `start`, on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

fn factory() -> Result<IWICImagingFactory, String> {
    // SAFETY: COM is started by the caller.
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("Windows' image component is not available: {e}"))
}

/// Whether some installed WIC decoder reads HEIF.
fn has_heif_decoder(f: &IWICImagingFactory) -> bool {
    // SAFETY: COM calls on live objects; every out value is checked.
    unsafe {
        let Ok(list) =
            f.CreateComponentEnumerator(WICDecoder.0 as u32, WICComponentEnumerateDefault.0 as u32)
        else {
            return false;
        };
        let list: IEnumUnknown = list;
        loop {
            let mut item = [None];
            let mut got = 0u32;
            if list.Next(&mut item, Some(&mut got)).is_err() || got == 0 {
                return false;
            }
            let Some(unknown) = item[0].take() else {
                return false;
            };
            let Ok(info) = windows::core::Interface::cast::<IWICBitmapDecoderInfo>(&unknown) else {
                continue;
            };
            if info
                .GetContainerFormat()
                .is_ok_and(|g| g == GUID_ContainerFormatHeif)
            {
                return true;
            }
        }
    }
}

/// Whether HEIC photos can be decoded here; if not, what to install.
pub fn available() -> Result<(), String> {
    let _com = Com::start()?;
    let f = factory()?;
    if has_heif_decoder(&f) {
        Ok(())
    } else {
        Err(HINT.into())
    }
}

fn describe(e: &windows::core::Error) -> String {
    format!("{} (0x{:08X})", e.message().trim(), e.code().0 as u32)
}

fn classify(e: windows::core::Error, what: &str) -> DecodeError {
    let code = e.code();
    if code == COMPONENT_NOT_FOUND || code == UNKNOWN_IMAGE_FORMAT || code == E_NOT_IMPL {
        DecodeError::Missing(HINT.into())
    } else {
        DecodeError::Damaged(format!("{what}: {}", describe(&e)))
    }
}

/// Decodes the primary image to 8-bit RGB, with no colour conversion.
pub fn decode(path: &Path) -> Result<Decoded, DecodeError> {
    let _com = Com::start().map_err(DecodeError::Missing)?;
    let f = factory().map_err(DecodeError::Missing)?;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(DecodeError::Damaged("the file name is not valid".into()));
    }
    wide.push(0);
    // SAFETY: `wide` is NUL-ended and outlives the calls; every out value is checked.
    unsafe {
        let decoder = f
            .CreateDecoderFromFilename(
                PCWSTR(wide.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .map_err(|e| {
                if has_heif_decoder(&f) {
                    classify(e, "the photo could not be opened")
                } else {
                    DecodeError::Missing(HINT.into())
                }
            })?;
        let frame = decoder
            .GetFrame(0)
            .map_err(|e| classify(e, "the photo could not be read"))?;
        let rgb = WICConvertBitmapSource(&GUID_WICPixelFormat24bppRGB, &frame)
            .map_err(|e| classify(e, "the photo's colours could not be read"))?;
        let (mut w, mut h) = (0u32, 0u32);
        rgb.GetSize(&mut w, &mut h)
            .map_err(|e| classify(e, "the photo's size could not be read"))?;
        let stride = (w as usize)
            .checked_mul(3)
            .ok_or_else(|| DecodeError::Damaged("the photo is too large".into()))?;
        let total = stride
            .checked_mul(h as usize)
            .filter(|t| *t > 0 && *t <= 1 << 30)
            .ok_or_else(|| DecodeError::Unsupported("the photo's size is not supported".into()))?;
        let mut pixels = vec![0u8; total];
        rgb.CopyPixels(std::ptr::null(), stride as u32, &mut pixels)
            .map_err(|e| classify(e, "the photo could not be decoded"))?;
        Ok(Decoded {
            width: w,
            height: h,
            rgb: pixels,
            upright: UPRIGHT,
        })
    }
}

/// WIC's HEIF decoder applies the file's rotation (`irot`) and mirror (`imir`) itself: the
/// tests convert a rotated and a mirrored fixture and check the result.
const UPRIGHT: bool = true;
