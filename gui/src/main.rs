//! A libadwaita frontend for iso2god-next.
//!
//! The window lives in [`app`]; all conversion logic comes from the `iso2god_next`
//! crate, and background work is isolated in [`task`].

// Don't open a console window next to the app on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod format;
mod ftp;
mod model;
mod row;
mod settings;
mod settings_dialog;
mod task;

use std::path::PathBuf;

use relm4::RelmApp;

/// Reverse-DNS application ID, used for the window's app icon and actions.
pub const APP_ID: &str = "io.github.georgewoodall82.iso2god-next";

fn main() {
    let (program, init) = parse_args();

    // Parse our own arguments; GApplication must not see the file list.
    let app = RelmApp::new(APP_ID).with_args(vec![program]);
    app.run::<app::AppModel>(init);
}

fn parse_args() -> (String, app::AppInit) {
    let mut args = std::env::args_os();
    let program = args
        .next()
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_else(|| "iso2god-next-gui".to_owned());

    let mut init = app::AppInit::default();
    while let Some(arg) = args.next() {
        let text = arg.to_str().map(str::to_owned);
        match text.as_deref() {
            Some("--output") | Some("-o") => {
                if let Some(dir) = args.next() {
                    init.output_dir = Some(PathBuf::from(dir));
                }
            }
            Some("--help") | Some("-h") => {
                println!("Usage: iso2god-next-gui [OPTIONS] [ISO_FILE|FOLDER…]");
                println!();
                println!("Options:");
                println!(
                    "  -o, --output <DIR>  Preselect the folder converted games are written to"
                );
                println!("  -h, --help          Print this help");
                std::process::exit(0);
            }
            Some(value) if value.starts_with('-') => {}
            _ => init.files.push(PathBuf::from(arg)),
        }
    }

    (program, init)
}
