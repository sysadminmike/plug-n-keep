<p align="center">
  <img src="app/assets/icon.png" alt="" width="128">
</p>

# Building Plug-n-Keep

## Common

The toolchain is pinned in `rust-toolchain.toml`: Rust 1.100.0 with the `clippy` and `rustfmt`
components, minimal profile. Rustup installs it on first use.

```text
cargo build --release
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The release build produces `plug-n-keep.exe` on Windows and `plug-n-keep` on Linux and FreeBSD.

## Testing

```text
cargo test --workspace
cargo test --workspace --features plug-n-keep/fake-device
```

The tests use a fake device and never touch a real one.

To try the window without a device, run a debug build with the fake device and point it at any
folder:

```text
cargo run --features plug-n-keep/fake-device -- --fake-device <folder> --state-dir <folder>
```

## Windows

The repository does not pin a Windows target, so it builds for your Rust installation's default
target. Both are supported: with the GNU target (`x86_64-pc-windows-gnu`) the app icon is compiled
in with `windres`, which needs a MinGW toolchain; with MSVC it uses `rc.exe`. Set `WINDRES` or `RC`
to use a particular tool. If neither is found the build still succeeds, with a warning, and the exe
just has no file icon.

At run time:

- Phones and cameras are read through Windows Portable Devices. An iPhone needs Apple's driver, and
  must be unlocked and must trust the PC (its own "Trust This Computer?" dialog).
- JPG copies of HEIC photos are made with the system's decoder (WIC).
- Settings are kept in `%APPDATA%\plug-n-keep`.

## Linux

On Ubuntu (24.04 or later), install a C toolchain and the development packages for the window
system:

```text
sudo apt-get install build-essential pkg-config libxkbcommon-dev libwayland-dev \
    libgl1-mesa-dev libx11-dev
```

No phone library is needed to build or to run. The app always works with USB sticks and drives.
Phone and camera libraries are loaded when first needed; without them the app simply has no phone
or camera support, and the device list says which package to install.

```text
sudo apt install libmtp9 libimobiledevice6 usbmuxd
sudo apt install libgphoto2-6t64
```

- `libmtp9` is for Android phones (named `libmtp9t64` on Ubuntu 24.04). The phone must be set to
  "File transfer".
- `libimobiledevice6` (which brings libplist) and `usbmuxd` are for iPhones. The `usbmuxd` service
  must be running, and the iPhone unlocked and trusted. The only change a connection can cause is
  the pairing record made by the iPhone's own "Trust This Computer?" dialog.
- `libgphoto2-6t64` (`libgphoto2-6` before 24.04) is for still cameras (PTP). A camera is read-only
  through it: the app only lists folders and files and downloads files, and never deletes, uploads,
  captures or changes a setting. A camera is listed once: devices libmtp already lists as a phone,
  iPhones, drives and anything that isn't a still camera are left out. A camera that shows up as a
  drive is read as a drive, with or without the library.
- `libheif` (Ubuntu: `libheif1` and `libheif-plugin-libde265`) is optional too, and is used for JPG copies of HEIC photos.

If the desktop's file manager holds a phone or camera, let it go there first.

Other notes:

- Eject for sticks and drives goes through udisks2, which the desktop installs. It unmounts each
  mounted partition and powers the drive off. It refuses a drive that holds the running system (a
  live USB: `/`, `/boot`, `/usr` or `/home`) and one with an encrypted or LVM volume still open;
  close that first. Without udisks2 running, the Eject button says so.
- New devices are noticed through the kernel's uevent socket, with a slow check of the mounted
  volumes alongside.
- On Wayland without graphics acceleration (a virtual machine, say) the app starts itself again on
  X11 automatically.
- Settings are kept in `$XDG_CONFIG_HOME/plug-n-keep`, or `~/.config/plug-n-keep`.

## FreeBSD

It builds from source; the file-system code is plain POSIX. USB sticks and drives are listed and
read, and new ones are noticed by a slow check of the mounted volumes. A USB disk the system runs
from is never offered. Phones, cameras and Eject are not available yet; use the system's own Eject.

To cross-check from another system:

```text
cargo check --target x86_64-unknown-freebsd --workspace
```
