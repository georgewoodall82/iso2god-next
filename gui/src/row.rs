//! One row in the game list: title, details, optional live progress and a remove
//! button. Rows are plain `AdwActionRow`s updated in place.

use std::path::PathBuf;

use iso2god_next::SourceKind;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::app::{AppModel, AppMsg};
use crate::format;
use crate::model::{EntryState, IsoEntry};

pub struct IsoRow {
    pub row: adw::ActionRow,
    pub path: PathBuf,
    progress: gtk::ProgressBar,
    percent: gtk::Label,
    icon: gtk::Image,
    remove: gtk::Button,
}

impl IsoRow {
    pub fn new(index: usize, entry: &IsoEntry, sender: &ComponentSender<AppModel>) -> Self {
        let icon = gtk::Image::from_icon_name(icon_for(entry));
        icon.set_valign(gtk::Align::Center);
        icon.add_css_class("dim-label");

        let percent = gtk::Label::builder()
            .width_chars(4)
            .xalign(1.0)
            .valign(gtk::Align::Center)
            .visible(false)
            .css_classes(["dim-label", "numeric"])
            .build();

        let progress = gtk::ProgressBar::builder()
            .width_request(132)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();

        let remove = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Remove from the list")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();

        let row = adw::ActionRow::builder()
            .title(entry.title())
            .subtitle(subtitle(entry))
            .build();

        row.add_prefix(&icon);
        row.add_suffix(&percent);
        row.add_suffix(&progress);
        row.add_suffix(&remove);

        let sender = sender.clone();
        remove.connect_clicked(move |_| sender.input(AppMsg::Remove(index)));

        let iso_row = IsoRow {
            row,
            path: entry.path.clone(),
            progress,
            percent,
            icon,
            remove,
        };
        iso_row.update(entry, false);
        iso_row
    }

    pub fn update(&self, entry: &IsoEntry, app_converting: bool) {
        self.row.set_title(&entry.title());
        self.row.set_subtitle(&subtitle(entry));

        let running = match &entry.state {
            EntryState::Converting(progress) => Some(progress.overall_fraction()),
            EntryState::Uploading(progress) => Some(progress.fraction()),
            _ => None,
        };

        self.progress.set_visible(running.is_some());
        self.percent.set_visible(running.is_some());
        if let Some(fraction) = running {
            self.progress.set_fraction(fraction);
            self.percent
                .set_label(&format!("{}%", format::percent(fraction)));
        }

        self.icon.set_icon_name(Some(icon_for(entry)));
        self.icon.set_css_classes(icon_classes(&entry.state));
        self.remove.set_sensitive(
            !app_converting
                && !matches!(
                    entry.state,
                    EntryState::Queued | EntryState::Converting(_) | EntryState::Uploading(_)
                ),
        );

        self.row
            .set_tooltip_text(Some(&entry.path.display().to_string()));
    }
}

fn subtitle(entry: &IsoEntry) -> String {
    match &entry.state {
        EntryState::Inspecting => "Reading…".to_owned(),
        EntryState::Queued => "Waiting to convert…".to_owned(),
        EntryState::Converting(progress) => format!(
            "{} · {}%",
            progress.stage.label(),
            format::percent(progress.overall_fraction())
        ),
        EntryState::Uploading(progress) => {
            if progress.files_total == 0 {
                "Connecting to the Xbox…".to_owned()
            } else if progress.files_done >= progress.files_total {
                "Uploaded to the Xbox".to_owned()
            } else if progress.file.is_empty() {
                format!(
                    "Uploading to the Xbox · {}%",
                    format::percent(progress.fraction())
                )
            } else {
                format!(
                    "Uploading {} · {}%",
                    progress.file,
                    format::percent(progress.fraction())
                )
            }
        }
        EntryState::Done { uploaded: true } => "Converted · uploaded to the Xbox".to_owned(),
        EntryState::Done { uploaded: false } => "Converted".to_owned(),
        EntryState::Cancelled => "Cancelled".to_owned(),
        EntryState::Failed(message) => format!("Failed: {message}"),
        EntryState::Ready => match &entry.info {
            Some(info) => format!(
                "{}{} · {} · Title ID {}",
                if info.kind == SourceKind::Folder {
                    "Folder · "
                } else {
                    ""
                },
                info.content_type.description(),
                format::size(info.data_size),
                info.title_id_string()
            ),
            None => "Ready".to_owned(),
        },
    }
}

fn icon_classes(state: &EntryState) -> &'static [&'static str] {
    match state {
        EntryState::Done { .. } => &["success"],
        EntryState::Failed(_) => &["error"],
        _ => &["dim-label"],
    }
}

fn icon_for(entry: &IsoEntry) -> &'static str {
    match &entry.state {
        EntryState::Inspecting | EntryState::Queued | EntryState::Converting(_) => {
            "view-refresh-symbolic"
        }
        EntryState::Uploading(_) => "go-up-symbolic",
        EntryState::Done { .. } => "object-select-symbolic",
        EntryState::Failed(_) => "dialog-error-symbolic",
        EntryState::Cancelled => "media-playback-stop-symbolic",
        EntryState::Ready if entry.is_folder() => "folder-symbolic",
        EntryState::Ready => "media-optical-symbolic",
    }
}
