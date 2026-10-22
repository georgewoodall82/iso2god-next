<h1 align="center">iso2god-next</h1>

<p align="center">
  Convert Xbox 360 and original Xbox ISO images, or plain game folders, into <strong>Games on Demand</strong> packages,<br>
  from a native desktop app or the command line, and send them straight to your console over FTP.
</p>

<p align="center">
  <img src="screenshots/screenshot.png" alt="iso2god-next converting an ISO image" width="640">
</p>

## Features

- **Desktop app** built with GTK4 and libadwaita: add several ISOs or game folders at once (buttons, drag and drop, or command line), see per-game and overall progress, and cancel at any time.
- **Game folders**: point it at a folder that contains a `default.xex` and it converts the files directly, with no need to rebuild an ISO first.
- **FTP upload** to the console: after each conversion the finished package is copied to the Xbox 360 (Aurora, Freestyle Dash, XeXMenu and other FTP servers), with its own progress bar.
- **Command line tool** for scripting and batch jobs.
- **Fast**: part files are written in parallel across worker threads.
- **Trimming**: unused space at the end of the ISO can be dropped, which usually saves several gigabytes.
- **Title lookup**: the game name is filled in automatically from the built-in title database, or you can set your own.
- Handles both Xbox 360 and original Xbox images.

## Download

Grab the latest build from the [releases page](https://github.com/georgewoodall82/iso2god-next/releases). File names include the version, shown here as `<version>` (for example `v1.1.1`).

| Platform | File | Status |
| -------- | ---- | ------ |
| Linux (x86_64) | `iso2god-next-<version>-linux-x86_64.AppImage` | Supported |
| Linux (aarch64) | `iso2god-next-<version>-linux-aarch64.AppImage` | Supported |
| Windows (x86_64) | `iso2god-next-<version>-windows-x86_64.zip` | **Experimental** |
| macOS (Apple silicon) | `iso2god-next-<version>-macos-aarch64.dmg` | **Experimental** |
| macOS (Intel) | `iso2god-next-<version>-macos-x86_64.dmg` | **Experimental** |

### Linux

Make the AppImage executable and start it:

```sh
chmod +x iso2god-next-*-linux-x86_64.AppImage
./iso2god-next-*-linux-x86_64.AppImage
```

The AppImage is built on Ubuntu 24.04, so it needs a distribution with glibc 2.39 or newer (Ubuntu 24.04, Fedora 40, Debian 13 and later).

### Windows (experimental)

Unzip the archive anywhere and run `iso2god-next-gui.exe`. The program is not code-signed, so Windows SmartScreen may warn you the first time: choose **More info → Run anyway**.

### macOS (experimental)

Open the disk image and drag **ISO2GOD Next** into Applications. The app is only ad-hoc signed, so macOS refuses to open it by double-clicking the first time: right-click it, choose **Open**, then confirm.

The Windows and macOS builds are produced automatically but have had little testing on real machines. If the window does not open or looks wrong, please [open an issue](https://github.com/georgewoodall82/iso2god-next/issues) with your system version and any error text.

The command line tool is not part of the releases; build it from source (see [Command line](#command-line)).

## Desktop app

1. **Add games** with **Add ISOs** or **Add XEX Folder**, by dragging ISO files or folders into the window, or on the command line.
2. **Choose an output folder.**
3. **Convert.** Games are converted one after another, and a toast tells you when everything is done.
4. **Upload (optional).** Open **Settings → FTP upload**, enter the console's IP address and use **Test connection** to check it. The username and password default to `xboxftp`. Converted games are copied to `Hdd1:\Content\0000000000000000` unless you change the content folder.

| Shortcut | Action |
| -------- | ------ |
| <kbd>Ctrl</kbd>+<kbd>O</kbd> | Add ISOs |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd> | Add XEX folders |
| <kbd>Ctrl</kbd>+<kbd>Return</kbd> | Convert |
| <kbd>Esc</kbd> | Cancel the running conversion |
| <kbd>Ctrl</kbd>+<kbd>,</kbd> | Settings |
| <kbd>F1</kbd> | About |

### Game folders

A game folder is the contents of a disc or an extracted ISO: a `default.xex` (Xbox 360) or `default.xbe` (original Xbox) at the top level, next to the game's other files and subfolders. The app lays the folder out as the disc's data volume while it converts, so nothing extra is written to disk and the result is the same package an ISO would give.

- The output folder must not be inside the game folder.
- File and folder names have to be plain ASCII (at most 255 characters), no two names in a folder may differ only in letter case, and no single file can be 4 GiB or larger. These are limits of the disc format; the app tells you which file is the problem.
- `.DS_Store`, `Thumbs.db`, `desktop.ini` and `._*` files are left out.

FTP settings are saved to `iso2god-next/ftp.json` in your user configuration folder (`~/.config` on Linux and macOS, `%APPDATA%` on Windows). The password is stored in plain text. On Linux and macOS the file is created readable only by you.

### Building the app

You need Rust 1.85 or newer plus the GTK 4.10+ and libadwaita 1.5+ development files.

```sh
# Fedora
sudo dnf install gtk4-devel libadwaita-devel

# Debian / Ubuntu
sudo apt install libgtk-4-dev libadwaita-1-dev

# macOS (Homebrew)
brew install gtk4 libadwaita librsvg
```

```sh
git clone https://github.com/georgewoodall82/iso2god-next
cd iso2god-next
cargo run --release -p iso2god-next-gui
```

Optional arguments:

```sh
iso2god-next-gui [-o|--output DIR] [ISO_FILE|FOLDER ...]
```

[`gui/data/`](gui/data) contains a desktop entry and an icon if you want to install the app system-wide.

## Command line

The CLI has no GUI dependencies, so all it needs is Rust 1.85 or newer:

```sh
cargo build --release
./target/release/iso2god-next game.iso ./output

# or straight from a folder that contains default.xex
./target/release/iso2god-next ./MyGame ./output
```

```
Usage: iso2god-next [OPTIONS] <SOURCE> <DEST_DIR>

Arguments:
  <SOURCE>    ISO file, or folder holding a game's files (with a default.xex), to convert
  <DEST_DIR>  A folder to write resulting GOD files to

Options:
      --dry-run             Do not convert anything, just print the title info
      --game-title <TITLE>  Set game title
      --trim[=<TRIM>]       Whether to trim off unused space from the ISO image (folders have none);
                            passing no --trim flag at all is equivalent to "from-end" [possible values: from-end, none]
  -j, --num-threads <N>     Number of worker threads to use [default: 1]
  -h, --help                Print help (see more with '--help')
  -V, --version             Print version
```

`--dry-run` prints the title ID, name, type and the size (before and after trimming, for an ISO) without writing anything.

## Using the library

The conversion pipeline has no UI code in it, so you can call it from your own program:

```rust
use iso2god_next::{convert, ConvertOptions};

// the source is an ISO file or a folder with a default.xex
let options = ConvertOptions::new("game.iso", "output");
convert(&options, |progress| {
    println!("{}: {:.0}%", progress.stage.label(), progress.fraction() * 100.0);
})?;
```

`inspect()` reads an image's or folder's metadata without converting it, and `convert_cancellable()` takes a `CancelToken` so long conversions can be stopped.

## Project layout

| Path | Purpose |
| ---- | ------- |
| `src/convert.rs` | The conversion pipeline: `inspect()`, `convert()`, progress and cancellation |
| `src/iso/`, `src/god/`, `src/executable/`, `src/game_list/` | ISO parsing, Games on Demand writing and the title database |
| `src/folder.rs` | Presents a game folder as an Xbox disc volume, built on the fly |
| `src/bin/iso2god-next.rs` | Command line frontend |
| `gui/src/app.rs` | Main window |
| `gui/src/row.rs` | One row per ISO, with its own progress bar |
| `gui/src/task.rs` | Background threads for inspection, conversion and upload |
| `gui/src/ftp.rs` | Minimal FTP client that copies a package to the console |
| `gui/src/settings.rs`, `gui/src/settings_dialog.rs` | Saved FTP settings and the dialog that edits them |
| `tests/convert.rs` | End-to-end tests using a generated synthetic ISO |

## Tests

```sh
cargo test --all
```

The integration tests build a tiny synthetic Xbox image (and a matching game folder), convert it, and check the resulting data file and CON header, so the whole pipeline is covered without a retail disc image. The FTP code is tested against an in-process mock server.

## Credits and licence

iso2god-next is a fork of [iso2god-rs](https://github.com/iliazeus/iso2god-rs) by Ilia Pozdnyakov, which is itself a rewrite of [iso2god-cli](https://github.com/eliecharra/iso2god-cli) by Elie Charra. The core conversion code comes from there; the desktop app, FTP upload and the reorganised library are new in this fork.

Released under the [MIT licence](LICENSE).
