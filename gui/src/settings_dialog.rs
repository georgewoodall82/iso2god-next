//! The preferences dialog for the FTP upload settings.

use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::Duration;

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk::{self, glib};

use crate::ftp;
use crate::settings::FtpSettings;

/// How often the dialog checks whether the connection test has finished.
const TEST_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The editable rows; cheap to clone because GTK widgets are reference counted.
#[derive(Clone)]
struct Rows {
    enable: adw::SwitchRow,
    host: adw::EntryRow,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    remote_dir: adw::EntryRow,
}

impl Rows {
    fn read(&self) -> FtpSettings {
        FtpSettings {
            enabled: self.enable.is_active(),
            host: self.host.text().trim().to_owned(),
            username: self.username.text().trim().to_owned(),
            password: self.password.text().into(),
            remote_dir: self.remote_dir.text().trim().to_owned(),
        }
    }
}

/// Shows the dialog over `parent`. `on_closed` receives the edited settings
/// when the dialog closes; that keeps typing in the password row from touching
/// the disk on every keystroke.
pub fn present(
    parent: &impl IsA<gtk::Widget>,
    current: &FtpSettings,
    on_closed: impl Fn(FtpSettings) + 'static,
) {
    let dialog = adw::PreferencesDialog::builder().title("Settings").build();
    let page = adw::PreferencesPage::new();

    let ftp_group = adw::PreferencesGroup::builder()
        .title("FTP upload")
        .description("After converting an ISO image, upload it to the console over FTP.")
        .build();

    let rows = Rows {
        enable: adw::SwitchRow::builder()
            .title("Enable FTP upload")
            .subtitle("Send each converted game to the console after conversion")
            .active(current.enabled)
            .build(),
        host: adw::EntryRow::builder()
            .title("Console address")
            .text(&current.host)
            .build(),
        username: adw::EntryRow::builder()
            .title("Username")
            .text(&current.username)
            .build(),
        password: adw::PasswordEntryRow::builder()
            .title("Password")
            .text(&current.password)
            .build(),
        remote_dir: adw::EntryRow::builder()
            .title("Content folder")
            .text(&current.remote_dir)
            .build(),
    };
    rows.host.set_tooltip_text(Some(
        "IP address of the Xbox 360, for example 192.168.1.23 (port 21 is used)",
    ));
    rows.remote_dir.set_tooltip_text(Some(
        r"Folder on the console, usually Hdd1:\Content\0000000000000000",
    ));

    let test_row = adw::ActionRow::builder()
        .title("Test connection")
        .subtitle("Check the address, username and password")
        .build();
    let test_button = gtk::Button::builder()
        .label("Test")
        .valign(gtk::Align::Center)
        .build();
    test_row.add_suffix(&test_button);

    ftp_group.add(&rows.enable);
    ftp_group.add(&rows.host);
    ftp_group.add(&rows.username);
    ftp_group.add(&rows.password);
    ftp_group.add(&rows.remote_dir);
    ftp_group.add(&test_row);
    page.add(&ftp_group);
    dialog.add(&page);

    {
        let rows = rows.clone();
        let dialog = dialog.clone();
        test_button.connect_clicked(move |button| {
            let settings = FtpSettings {
                enabled: true,
                ..rows.read()
            };
            start_connection_test(&dialog, button, settings);
        });
    }

    dialog.connect_closed(move |_| on_closed(rows.read()));
    dialog.present(Some(parent));
}

/// Runs the check on a worker thread so a wrong address cannot freeze the
/// dialog for the whole connect timeout.
fn start_connection_test(
    dialog: &adw::PreferencesDialog,
    button: &gtk::Button,
    settings: FtpSettings,
) {
    if !settings.is_configured() {
        dialog.add_toast(adw::Toast::new("Enter the console address first."));
        return;
    }

    button.set_sensitive(false);
    button.set_label("Testing…");

    let (results, received) = mpsc::channel();
    thread::spawn(move || {
        let _ = results.send(ftp::test_connection(&settings));
    });

    let button = button.clone();
    let dialog = dialog.clone();
    glib::timeout_add_local(TEST_POLL_INTERVAL, move || {
        let result = match received.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("the connection test stopped unexpectedly"))
            }
        };

        button.set_label("Test");
        button.set_sensitive(true);

        let toast = match result {
            Ok(message) => adw::Toast::new(&message),
            Err(error) => adw::Toast::new(&format!("Connection failed: {error:#}")),
        };
        dialog.add_toast(toast);
        glib::ControlFlow::Break
    });
}
