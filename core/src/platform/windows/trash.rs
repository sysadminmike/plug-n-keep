//! Sending a file to the Recycle Bin, and never deleting it for good.
//!
//! The shell can quietly turn "send to the Recycle Bin" into a permanent delete: when the drive
//! has no bin, the file is too big for it, or the file is on a network share, and `FOF_NO_UI`
//! answers its "delete permanently?" question with Yes. So this module does not trust the
//! flags alone. It uses `IFileOperation` with a progress sink that checks, for the one file:
//!
//! - `PreDeleteItem`: `dwFlags` must contain `TSF_DELETE_RECYCLE_IF_POSSIBLE`. Windows clears
//!   it when it would destroy the file rather than recycle it. If it is missing, the sink
//!   returns an error, which stops that delete before anything is touched.
//! - `PostDeleteItem`: the delete must have succeeded *and* the bin's new item
//!   (`psiNewlyCreated`) must exist. Only then is the file counted as recycled.
//!
//! The call succeeds only if both held, the operation reported no error and no abort, and the
//! file is gone from its place. Otherwise the caller gets an error and the file stays.
//!
//! The sink is both `Advise`d on the operation (so it sees every item the shell handles) and
//! passed with the item. It only sets flags, so seeing one delete by both routes counts it once.
//!
//! `FOF_WANTNUKEWARNING` is deliberately not used. It "partly overrides `FOF_NOCONFIRMATION`":
//! the shell would then show a "permanently delete?" dialog. This module runs on a short-lived
//! thread with no window, often with nobody at the screen, and a dialog there can wait for ever.
//! It would also add nothing: `PreDeleteItem` already stops any delete that would destroy the
//! file, before the shell gets as far as asking. The guard is the safety; the flag would only
//! add a way to hang.
//!
//! The work runs on its own short-lived thread with single-threaded COM (the shell's
//! `IFileOperation` is apartment-threaded), so it behaves the same whatever apartment the
//! caller's thread is in.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ::windows::Win32::Foundation::{E_ABORT, RPC_E_CHANGED_MODE};
use ::windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    IBindCtx,
};
use ::windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NO_UI, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation,
    IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
    SHCreateItemFromParsingName, TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use ::windows::core::{HRESULT, PCWSTR, implement};

/// What the sink saw, shared with the code that started the operation.
#[derive(Default)]
struct Seen {
    /// `PreDeleteItem` found Windows would not recycle the file, and said no.
    refused: AtomicBool,
    /// `PostDeleteItem` reported success with an item in the bin.
    recycled: AtomicBool,
}

/// Whether Windows says it will recycle (and not destroy) the file: the flag it passes to
/// `PreDeleteItem`.
pub(crate) fn will_recycle(flags: u32) -> bool {
    flags & (TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32) != 0
}

/// Whether a finished delete really put the file in the bin: it succeeded, and the bin's new
/// item exists.
pub(crate) fn was_recycled(result: HRESULT, bin_item_exists: bool) -> bool {
    result.is_ok() && bin_item_exists
}

#[implement(IFileOperationProgressSink)]
struct Sink(Arc<Seen>);

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for Sink_Impl {
    fn StartOperations(&self) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _: HRESULT) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        _: &PCWSTR,
    ) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Option<&IShellItem>,
    ) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        _: Option<&IShellItem>,
        _: &PCWSTR,
    ) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        _: Option<&IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Option<&IShellItem>,
    ) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        _: Option<&IShellItem>,
        _: &PCWSTR,
    ) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        _: Option<&IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Option<&IShellItem>,
    ) -> ::windows::core::Result<()> {
        Ok(())
    }
    /// The guard: refuse anything that is not a recycle.
    fn PreDeleteItem(&self, flags: u32, _: Option<&IShellItem>) -> ::windows::core::Result<()> {
        if will_recycle(flags) {
            Ok(())
        } else {
            self.0.refused.store(true, Ordering::SeqCst);
            Err(E_ABORT.into())
        }
    }
    fn PostDeleteItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        result: HRESULT,
        bin_item: Option<&IShellItem>,
    ) -> ::windows::core::Result<()> {
        if was_recycled(result, bin_item.is_some()) {
            self.0.recycled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn PreNewItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        _: &PCWSTR,
    ) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: Option<&IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Option<&IShellItem>,
    ) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> ::windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> ::windows::core::Result<()> {
        Ok(())
    }
}

/// The path as the shell wants it: the `\\?\` prefix that `canonicalize` adds is taken off.
fn shell_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// The shell's file operations work on paths of at most `MAX_PATH` (260, with the final NUL).
const MAX_PATH_CHARS: usize = 259;

fn too_long_for_the_shell(path: &Path) -> bool {
    path.as_os_str().encode_wide().count() > MAX_PATH_CHARS
}

/// Send `path` to the Recycle Bin. Never deletes it: an error means the file is still there.
pub fn send_to_bin(path: &Path) -> Result<(), String> {
    let path = shell_path(path);
    if too_long_for_the_shell(&path) {
        return Err(format!(
            "its path is longer than the {} limit of {MAX_PATH_CHARS} characters, which the shell can't handle",
            super::super::BIN_NAME
        ));
    }
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() && !super::fs::is_link(&m) => {}
        Ok(_) => return Err("it is not a plain file".into()),
        Err(e) => return Err(format!("could not look at it: {e}")),
    }
    let worker = {
        let path = path.clone();
        std::thread::spawn(move || recycle(&path))
    };
    let outcome = worker
        .join()
        .unwrap_or_else(|_| Err("the Recycle Bin call failed unexpectedly".into()));
    // Whatever happened, the file must now be gone from its place if this is to count.
    let still_there = std::fs::symlink_metadata(&path).is_ok();
    match (outcome, still_there) {
        (Ok(()), false) => Ok(()),
        (Ok(()), true) => Err("Windows said it was recycled but the file is still there".into()),
        (Err(e), _) => Err(e),
    }
}

/// Runs on its own thread, with COM set up for the shell.
fn recycle(path: &Path) -> Result<(), String> {
    // SAFETY: plain per-thread COM setup; undone below when it succeeded.
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let balance = if init.is_ok() {
        true
    } else if init == RPC_E_CHANGED_MODE {
        false
    } else {
        return Err(format!(
            "could not start the shell: {}",
            ::windows::core::Error::from(init)
        ));
    };
    let result = recycle_in_com(path);
    if balance {
        // SAFETY: pairs with the successful CoInitializeEx above, on this thread.
        unsafe { CoUninitialize() };
    }
    result
}

fn recycle_in_com(path: &Path) -> Result<(), String> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let seen = Arc::new(Seen::default());
    // SAFETY: COM is initialised on this thread; `wide` is NUL-terminated and outlives the
    // calls; the sink and the operation are released when they go out of scope.
    let performed = unsafe {
        (|| -> ::windows::core::Result<bool> {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOperationFlags(::windows::Win32::UI::Shell::FILEOPERATION_FLAGS(
                FOF_ALLOWUNDO.0 | FOF_NO_UI.0 | FOFX_RECYCLEONDELETE.0 | FOFX_EARLYFAILURE.0,
            ))?;
            let item: IShellItem =
                SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>)?;
            let sink: IFileOperationProgressSink = Sink(seen.clone()).into();
            // Advise the sink for the whole operation (it sees every item, including any the
            // shell adds), and also pass it per item. `Seen` only sets flags, so seeing the
            // same delete through both routes cannot count a file twice.
            let cookie = op.Advise(&sink)?;
            let ran = (|| -> ::windows::core::Result<bool> {
                op.DeleteItem(&item, &sink)?;
                op.PerformOperations()?;
                Ok(op.GetAnyOperationsAborted()?.as_bool())
            })();
            let _ = op.Unadvise(cookie);
            ran
        })()
    };
    if seen.refused.load(Ordering::SeqCst) {
        return Err(NO_BIN.into());
    }
    match performed {
        Ok(false) if seen.recycled.load(Ordering::SeqCst) => Ok(()),
        Ok(false) => Err(format!("{NO_BIN} (Windows didn't confirm it was recycled)")),
        Ok(true) => Err(format!("{NO_BIN} (Windows stopped the operation)")),
        Err(e) => Err(format!("{NO_BIN} ({e})")),
    }
}

/// Why a file was left: the bin can't take it.
const NO_BIN: &str = "Windows can't send it to the Recycle Bin (there may be none on this drive, or the file may be too big for it, or on a network share)";

#[cfg(test)]
mod tests {
    use super::*;
    use ::windows::Win32::Foundation::{E_FAIL, S_OK};

    #[test]
    fn only_a_delete_windows_will_recycle_is_allowed() {
        let recycle = TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32;
        assert!(will_recycle(recycle));
        assert!(will_recycle(recycle | 1));
        assert!(!will_recycle(0));
        assert!(!will_recycle(!recycle));
    }

    #[test]
    fn a_delete_counts_as_recycled_only_with_success_and_a_bin_item() {
        assert!(was_recycled(S_OK, true));
        assert!(
            !was_recycled(S_OK, false),
            "no item in the bin: it was destroyed"
        );
        assert!(!was_recycled(E_FAIL, true));
        assert!(!was_recycled(E_FAIL, false));
    }

    #[test]
    fn the_sink_refuses_a_delete_that_would_destroy_and_remembers_it() {
        let seen = Arc::new(Seen::default());
        let sink = Sink(seen.clone());
        let com: IFileOperationProgressSink = sink.into();
        // SAFETY: calls into our own sink object, which ignores the item.
        unsafe {
            assert!(com.PreDeleteItem(0, None::<&IShellItem>).is_err());
            assert!(seen.refused.load(Ordering::SeqCst));
            // And a post-delete with no bin item is not a recycle.
            com.PostDeleteItem(0, None::<&IShellItem>, S_OK, None::<&IShellItem>)
                .unwrap();
            assert!(!seen.recycled.load(Ordering::SeqCst));
        }
        let seen = Arc::new(Seen::default());
        let com: IFileOperationProgressSink = Sink(seen.clone()).into();
        // SAFETY: as above.
        unsafe {
            com.PreDeleteItem(TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32, None::<&IShellItem>)
                .unwrap();
        }
        assert!(!seen.refused.load(Ordering::SeqCst));
    }

    #[test]
    fn a_verbatim_path_is_given_to_the_shell_without_its_prefix() {
        assert_eq!(
            shell_path(Path::new(r"\\?\C:\a\b")),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(
            shell_path(Path::new(r"\\?\UNC\srv\share\f")),
            PathBuf::from(r"\\srv\share\f")
        );
        assert_eq!(shell_path(Path::new(r"C:\a")), PathBuf::from(r"C:\a"));
    }

    #[test]
    fn a_path_over_max_path_is_refused_with_a_clear_message() {
        let long = PathBuf::from(format!(r"C:\{}\f.txt", "a".repeat(300)));
        assert!(too_long_for_the_shell(&long));
        let err = send_to_bin(&long).unwrap_err();
        assert!(err.contains("longer than"), "{err}");
        assert!(!too_long_for_the_shell(Path::new(r"C:\a\b.txt")));
    }

    #[test]
    fn a_folder_or_a_missing_file_is_refused_and_left() {
        let t = tempfile::tempdir().unwrap();
        assert!(send_to_bin(t.path()).is_err());
        assert!(t.path().exists());
        assert!(send_to_bin(&t.path().join("nothing")).is_err());
    }

    /// A file on a network path has no Recycle Bin, so Windows would delete it for good. The
    /// guard must stop that, leaving the file. Uses this PC's own administrative share
    /// (`\\localhost\C$`), which Windows treats as a network path; skipped where it can't be
    /// reached (no admin rights, shares off).
    #[test]
    fn a_file_on_a_path_with_no_recycle_bin_is_left_not_destroyed() {
        let local = std::env::temp_dir().join(format!("pnk-nobin-{}", std::process::id()));
        std::fs::create_dir_all(&local).unwrap();
        let file = local.join("precious.txt");
        std::fs::write(&file, b"keep me").unwrap();
        let drive = file.to_string_lossy().chars().next().unwrap();
        let unc = PathBuf::from(format!(
            r"\\localhost\{drive}$\{}",
            file.to_string_lossy().get(3..).unwrap()
        ));
        if std::fs::symlink_metadata(&unc).is_err() {
            eprintln!("skipped: {} can't be reached", unc.display());
            let _ = std::fs::remove_dir_all(&local);
            return;
        }
        let result = send_to_bin(&unc);
        let kept = std::fs::read(&file);
        let _ = std::fs::remove_dir_all(&local);
        assert!(
            result.is_err(),
            "a network file must not be sent to a bin that isn't there"
        );
        assert_eq!(
            kept.unwrap(),
            b"keep me",
            "the file must still be there, whole"
        );
    }

    /// Manual: a file bigger than the Recycle Bin's quota must be left, not destroyed.
    /// Set `PNK_TEST_BIG_FILE_MB` to more than the bin's limit for the temp drive (the limit is
    /// in the drive's Recycle Bin properties; use a small one), then run
    /// `cargo test -p pnk-core bigger_than_the_bin -- --ignored`. Needs the owner's say-so:
    /// it creates a file of that size.
    #[test]
    #[ignore = "manual: needs a file bigger than the bin's quota; ask the owner"]
    fn a_file_bigger_than_the_bin_is_left_not_destroyed() {
        let mb: u64 = std::env::var("PNK_TEST_BIG_FILE_MB")
            .expect("set PNK_TEST_BIG_FILE_MB to more than the bin's limit")
            .parse()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("pnk-bigbin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.bin");
        let f = std::fs::File::create(&file).unwrap();
        f.set_len(mb * 1024 * 1024).unwrap();
        drop(f);
        let r = send_to_bin(&file);
        let kept = file.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_err(), "too big for the bin: it must be refused");
        assert!(kept, "and the file must still be there");
    }

    /// Manual: a drive with no Recycle Bin (a removable USB stick). Set `PNK_TEST_NO_BIN_DIR`
    /// to an empty folder on such a drive, then run
    /// `cargo test -p pnk-core no_bin_drive -- --ignored`. Needs the owner's say-so.
    #[test]
    #[ignore = "manual: needs a removable drive; ask the owner"]
    fn a_file_on_a_drive_with_no_bin_is_left_not_destroyed() {
        let dir = PathBuf::from(
            std::env::var("PNK_TEST_NO_BIN_DIR").expect("set PNK_TEST_NO_BIN_DIR to a folder"),
        )
        .join(format!("pnk-nobin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("precious.txt");
        std::fs::write(&file, b"keep me").unwrap();
        let r = send_to_bin(&file);
        let kept = std::fs::read(&file).ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_err(), "no bin on this drive: it must be refused");
        assert_eq!(kept.as_deref(), Some(&b"keep me"[..]));
    }

    /// Puts one tiny file in the real Recycle Bin, so it is run on request only:
    /// `cargo test -p pnk-core recycles -- --ignored`.
    #[test]
    #[ignore = "puts a small file in the real Recycle Bin"]
    fn a_local_file_really_goes_to_the_recycle_bin() {
        let dir = std::env::temp_dir().join(format!("pnk-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pnk-recycle-test.txt");
        std::fs::write(&file, b"test").unwrap();
        let r = send_to_bin(&file);
        let gone = !file.exists();
        let _ = std::fs::remove_dir_all(&dir);
        r.unwrap();
        assert!(gone);
    }
}
