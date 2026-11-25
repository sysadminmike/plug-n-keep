<p align="center">
  <img src="assets/logo.png" alt="Plug-n-Keep" width="300">
</p>

<p align="center"><em>Plug it in. Keep everything.</em></p>

Plug-n-Keep is an app for Windows and Linux that backs up a USB device to a folder on the PC and keeps
everything that was ever on it. A sync copies what is new to the backup folder. Files deleted from the
device are kept too. Nothing is ever deleted for good, and nothing on the device is changed.

<p align="center"><a href="assets/screenshot.png"><img src="assets/screenshot.png" alt="Plug-n-Keep backing up an iPhone" width="800"></a></p>

## Supported devices

| | Windows | Linux | FreeBSD |
|---|---|---|---|
| USB sticks, cards and drives | yes | yes | yes |
| Android and iPhone | yes | yes (optional libraries) | no |
| Still cameras | yes | yes (optional library) | no |
| Eject | yes | yes | no, use the system's own |

A device with several storages (an Android phone with an SD card) gets a folder in the backup for each.

## Using it

A scan shows what's new, changed and deleted before anything is copied.

<p align="center"><a href="assets/screenshot-scan.png"><img src="assets/screenshot-scan.png" alt="A scan: what's new, changed and deleted, before anything is copied" width="800"></a></p>

Each device has its own settings, including what happens to deleted files.

<p align="center"><a href="assets/screenshot-settings.png"><img src="assets/screenshot-settings.png" alt="A device's settings, including what happens to deleted files" width="800"></a></p>

- **Scan** only reads the device. Nothing is copied or archived until you press **Start sync**.
  **Stop** and **Skip current** leave no half-written file.
- **Eject** (sticks, cards and drives) lets go of the device, then asks the system to eject it, and
  says when it is safe to unplug.
- **Add an existing backup…** picks a backup folder, or a folder that holds some, and each device in
  it is set up again from the files in it.
- **Forget** only takes a device off the app's list. Its backup folder and everything in it stay,
  and **Add an existing backup…** brings it back.
- **Deleted files** are moved into an archive folder inside the backup by default. For each device
  you can instead leave them where they are, or send them to the Recycle Bin (the Trash on Linux and
  FreeBSD). The old copy of a changed file is handled the same way. Nothing is ever permanently
  deleted: if a file can't go to the bin, it stays where it is.

### Where it keeps its state

Everything that matters about a device (its settings, what recognises it, its sync history and its
skip list) is in its backup folder, in hidden `.plug-n-keep-*` files. The app's own settings folder
only remembers where the backup folders are and what new devices start with. Back up the backup
folders and you have backed up everything.

### Moving to a new computer

1. Copy the backup folder (or the folder that holds all of them) to the new computer.
2. Run Plug-n-Keep. Nothing else needs installing or restoring.
3. Click **Add an existing backup…** and pick the folder. Each device comes back with its settings
   and history.

## Build

```text
cargo build --release
```

See [BUILD.md](BUILD.md) for each platform's requirements, optional phone and camera libraries,
testing and the fake device.

## More

The design is in [DESIGN.md](DESIGN.md).

## Built with

- [egui and eframe](https://github.com/emilk/egui) draw the window (MIT or Apache-2.0). The fonts
  egui ships are under the SIL Open Font Licence and the Ubuntu Font Licence.
- [windows](https://github.com/microsoft/windows-rs) (MIT or Apache-2.0) reaches the Windows
  portable-device (WPD) and shell APIs; [zbus](https://github.com/dbus2/zbus) (MIT) asks udisks2 to
  eject on Linux; [rfd](https://github.com/PolyMeilex/rfd) (MIT) shows the file dialogs.
- HEIC photos are converted to JPG with the system's own decoders (WIC on Windows, libheif loaded at
  run time on Linux) and [jpeg-encoder](https://github.com/vstroebel/jpeg-encoder) (MIT or
  Apache-2.0). This software is based in part on the work of the Independent JPEG Group.
- On Linux, libmtp, libimobiledevice with usbmuxd, libgphoto2 and libheif are optional, loaded at
  run time and not bundled.

The full list of third-party licences is in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Licence

MIT. See [LICENSE](LICENSE).
