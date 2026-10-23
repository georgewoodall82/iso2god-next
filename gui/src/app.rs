//! The application window: a list of ISO images and game folders, a destination
//! picker and the convert action with per-game and overall progress.
//!
//! All conversion work happens on background threads (see [`crate::task`]); this
//! component only reacts to messages and keeps the widgets in sync.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use iso2god_next::{CancelToken, ConversionProgress, IsoInfo};
use relm4::abstractions::Toaster;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk::{self, gio, pango};
use relm4::prelude::*;

use crate::APP_ID;
use crate::format;
use crate::ftp::UploadProgress;
use crate::model::{EntryState, IsoEntry};
use crate::row::IsoRow;
use crate::settings::FtpSettings;
use crate::settings_dialog;
use crate::task::{self, FileOutcome, Job};

#[derive(Debug)]
pub(crate) enum AppMsg {
    AddRequested,
    AddFolderRequested,
    FilesChosen(Vec<PathBuf>),
    Inspected(PathBuf, Result<Box<IsoInfo>, String>),
    Remove(usize),
    Clear,
    ChooseOutput,
    OutputChosen(PathBuf),
    Convert,
    Cancel,
    Progress(PathBuf, ConversionProgress),
    UploadStarted(PathBuf),
    UploadProgress(PathBuf, UploadProgress),
    FileDone(PathBuf, FileOutcome),
    AllDone,
    Settings,
    SettingsChanged(FtpSettings),
    About,
}

/// Startup state: ISO files and game folders passed on the command line and an optional
/// preselected output folder.
#[derive(Debug, Default)]
pub struct AppInit {
    pub files: Vec<PathBuf>,
    pub output_dir: Option<PathBuf>,
}

pub(crate) struct AppModel {
    entries: Vec<IsoEntry>,
    rows: RefCell<Vec<IsoRow>>,
    group: adw::PreferencesGroup,
    toaster: Toaster,
    window: adw::ApplicationWindow,
    output_dir: Option<PathBuf>,
    converting: bool,
    cancelling: bool,
    cancel: CancelToken,
    completed: usize,
    failed: usize,
    uploaded: usize,
    run_uploads: bool,
    active_job: Vec<PathBuf>,
    finished: bool,
    last_run_cancelled: bool,
    ftp: FtpSettings,
    clear_button: Option<gtk::Button>,
    add_action: Option<gio::SimpleAction>,
    add_folder_action: Option<gio::SimpleAction>,
    convert_action: Option<gio::SimpleAction>,
    cancel_action: Option<gio::SimpleAction>,
}

#[relm4::component(pub(crate))]
impl SimpleComponent for AppModel {
    type Init = AppInit;
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("ISO to GOD"),
            set_default_width: 900,
            set_default_height: 660,
            set_size_request: (480, 380),

            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                #[wrap(Some)]
                set_child = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        pack_start = &gtk::Button {
                            set_label: "Add ISOs",
                            set_tooltip_text: Some("Add ISO images (Ctrl+O)"),
                            set_action_name: Some("app.add"),
                        },
                        pack_start = &gtk::Button {
                            set_label: "Add XEX Folder",
                            set_tooltip_text: Some("Add folders that contain a default.xex (Ctrl+Shift+O)"),
                            set_action_name: Some("app.add-folder"),
                        },

                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle {
                            set_title: "ISO to GOD",
                            set_subtitle: "Xbox Games on Demand",
                        },

                        pack_end = &gtk::MenuButton {
                            set_icon_name: "open-menu-symbolic",
                            set_tooltip_text: Some("Main menu"),

                            #[wrap(Some)]
                            set_popover = &gtk::Popover {
                                gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_spacing: 2,
                                    set_margin_all: 6,

                                    gtk::Button {
                                        set_label: "Add ISO images",
                                        set_action_name: Some("app.add"),
                                        set_halign: gtk::Align::Start,
                                        add_css_class: "flat",
                                    },
                                    gtk::Button {
                                        set_label: "Add XEX folder",
                                        set_action_name: Some("app.add-folder"),
                                        set_halign: gtk::Align::Start,
                                        add_css_class: "flat",
                                    },
                                    gtk::Button {
                                        set_label: "Settings",
                                        set_action_name: Some("app.settings"),
                                        set_halign: gtk::Align::Start,
                                        add_css_class: "flat",
                                    },
                                    gtk::Button {
                                        set_label: "About ISO to GOD",
                                        set_action_name: Some("app.about"),
                                        set_halign: gtk::Align::Start,
                                        add_css_class: "flat",
                                    },
                                },
                            },
                        },
                    },

                    #[wrap(Some)]
                    set_content = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,

                        #[name = "empty_page"]
                        adw::StatusPage {
                            set_vexpand: true,
                            #[watch]
                            set_visible: model.entries.is_empty(),
                            set_icon_name: Some("media-optical-symbolic"),
                            set_title: "No games added",
                            set_description: Some(
                                "Add Xbox 360 or original Xbox ISO images, or folders that contain a game's files with its default.xex, choose an output folder, then convert them to Games on Demand format."
                            ),

                            #[wrap(Some)]
                            set_child = &gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_spacing: 18,
                                set_halign: gtk::Align::Center,

                                gtk::Box {
                                    set_orientation: gtk::Orientation::Horizontal,
                                    set_spacing: 12,
                                    set_halign: gtk::Align::Center,

                                    gtk::Button {
                                        set_label: "Add ISO images",
                                        set_action_name: Some("app.add"),
                                        add_css_class: "suggested-action",
                                    },
                                    gtk::Button {
                                        set_label: "Add XEX folder",
                                        set_action_name: Some("app.add-folder"),
                                    },
                                },

                                gtk::Button {
                                    set_label: "Choose output folder",
                                    set_action_name: Some("app.choose-output"),
                                    set_halign: gtk::Align::Center,
                                    add_css_class: "flat",
                                },

                                #[name = "empty_output_label"]
                                gtk::Label {
                                    #[watch]
                                    set_label: &model.empty_output_label(),
                                    set_ellipsize: pango::EllipsizeMode::Middle,
                                    set_max_width_chars: 56,
                                    add_css_class: "dim-label",
                                },
                            },
                        },

                        #[name = "prefs_page"]
                        adw::PreferencesPage {
                            #[watch]
                            set_visible: !model.entries.is_empty(),

                            #[name = "iso_group"]
                            adw::PreferencesGroup {
                                set_title: "Games",
                                set_description: Some("Images and folders are converted one after another into the output folder."),
                            },

                            #[name = "destination_group"]
                            adw::PreferencesGroup {
                                set_title: "Destination",

                                #[name = "output_row"]
                                adw::ActionRow {
                                    set_title: "Output folder",
                                    #[watch]
                                    set_subtitle: &model.output_subtitle(),
                                    #[watch]
                                    set_activatable: !model.converting,
                                    connect_activated[sender] => move |_| {
                                        sender.input(AppMsg::ChooseOutput);
                                    },

                                    add_suffix = &gtk::Button {
                                        set_label: "Browse…",
                                        set_valign: gtk::Align::Center,
                                        connect_clicked[sender] => move |_| {
                                            sender.input(AppMsg::ChooseOutput);
                                        },
                                    },
                                },
                            },
                        },
                    },

                    #[name = "bottom_bar"]
                    add_bottom_bar = &gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 12,
                        set_margin_all: 12,
                        #[watch]
                        set_visible: !model.entries.is_empty(),

                        #[name = "status_label"]
                        gtk::Label {
                            add_css_class: "dim-label",
                            #[watch]
                            set_label: &model.status_text(),
                            set_hexpand: true,
                            set_xalign: 0.0,
                            set_ellipsize: pango::EllipsizeMode::End,
                            set_max_width_chars: 34,
                        },

                        #[name = "overall_bar"]
                        gtk::ProgressBar {
                            set_hexpand: true,
                            set_valign: gtk::Align::Center,
                            set_show_text: false,
                            #[watch]
                            set_visible: model.converting,
                            #[watch]
                            set_fraction: model.overall_fraction(),
                        },

                        #[name = "cancel_button"]
                        gtk::Button {
                            set_label: "Cancel",
                            #[watch]
                            set_visible: model.converting,
                            #[watch]
                            set_sensitive: !model.cancelling,
                            #[watch]
                            set_label: model.cancel_label(),
                            connect_clicked[sender] => move |_| {
                                sender.input(AppMsg::Cancel);
                            },
                        },

                        #[name = "convert_button"]
                        gtk::Button {
                            set_label: "Convert",
                            #[watch]
                            set_sensitive: model.can_convert(),
                            #[watch]
                            set_visible: !model.converting,
                            #[watch]
                            set_label: &model.convert_label(),
                            add_css_class: "suggested-action",
                            connect_clicked[sender] => move |_| {
                                sender.input(AppMsg::Convert);
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for path in init.files {
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            if (path.is_file() || path.is_dir()) && seen.insert(path.clone()) {
                entries.push(IsoEntry::new(path));
            }
        }
        let pending: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();

        let mut model = AppModel {
            entries,
            rows: RefCell::new(Vec::new()),
            group: adw::PreferencesGroup::new(),
            toaster: Toaster::default(),
            window: root.clone(),
            output_dir: init.output_dir,
            converting: false,
            cancelling: false,
            cancel: CancelToken::new(),
            completed: 0,
            failed: 0,
            uploaded: 0,
            run_uploads: false,
            active_job: Vec::new(),
            finished: false,
            last_run_cancelled: false,
            ftp: FtpSettings::load(),
            clear_button: None,
            add_action: None,
            add_folder_action: None,
            convert_action: None,
            cancel_action: None,
        };

        let toast_overlay = model.toaster.overlay_widget();
        let widgets = view_output!();
        model.group = widgets.iso_group.clone();

        // Extra actions in the "Games" group header.
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let add_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add ISO images")
            .css_classes(["flat"])
            .build();
        add_button.set_action_name(Some("app.add"));
        let add_folder_button = gtk::Button::builder()
            .icon_name("folder-new-symbolic")
            .tooltip_text("Add XEX folders")
            .css_classes(["flat"])
            .build();
        add_folder_button.set_action_name(Some("app.add-folder"));
        let clear_button = gtk::Button::builder()
            .label("Clear")
            .tooltip_text("Remove all games from the list")
            .css_classes(["flat"])
            .build();
        let clear_sender = sender.clone();
        clear_button.connect_clicked(move |_| clear_sender.input(AppMsg::Clear));
        controls.append(&add_button);
        controls.append(&add_folder_button);
        controls.append(&clear_button);
        widgets.iso_group.set_header_suffix(Some(&controls));
        model.clear_button = Some(clear_button);

        // Application actions, shared by the header bar, menu and empty state.
        let app = relm4::main_adw_application();

        let add_action = gio::SimpleAction::new("add", None);
        let add_sender = sender.clone();
        add_action.connect_activate(move |_, _| add_sender.input(AppMsg::AddRequested));
        app.add_action(&add_action);
        app.set_accels_for_action("app.add", &["<primary>o"]);
        model.add_action = Some(add_action);

        let add_folder_action = gio::SimpleAction::new("add-folder", None);
        let add_folder_sender = sender.clone();
        add_folder_action
            .connect_activate(move |_, _| add_folder_sender.input(AppMsg::AddFolderRequested));
        app.add_action(&add_folder_action);
        app.set_accels_for_action("app.add-folder", &["<primary><shift>o"]);
        model.add_folder_action = Some(add_folder_action);

        let choose_action = gio::SimpleAction::new("choose-output", None);
        let choose_sender = sender.clone();
        choose_action.connect_activate(move |_, _| choose_sender.input(AppMsg::ChooseOutput));
        app.add_action(&choose_action);

        let convert_action = gio::SimpleAction::new("convert", None);
        let convert_sender = sender.clone();
        convert_action.connect_activate(move |_, _| convert_sender.input(AppMsg::Convert));
        app.add_action(&convert_action);
        app.set_accels_for_action("app.convert", &["<primary>Return"]);
        model.convert_action = Some(convert_action);

        let cancel_action = gio::SimpleAction::new("cancel", None);
        let cancel_sender = sender.clone();
        cancel_action.connect_activate(move |_, _| cancel_sender.input(AppMsg::Cancel));
        app.add_action(&cancel_action);
        app.set_accels_for_action("app.cancel", &["Escape"]);
        model.cancel_action = Some(cancel_action);

        let settings_action = gio::SimpleAction::new("settings", None);
        let settings_sender = sender.clone();
        settings_action.connect_activate(move |_, _| settings_sender.input(AppMsg::Settings));
        app.add_action(&settings_action);
        app.set_accels_for_action("app.settings", &["<primary>comma"]);

        let about_action = gio::SimpleAction::new("about", None);
        let about_sender = sender.clone();
        about_action.connect_activate(move |_, _| about_sender.input(AppMsg::About));
        app.add_action(&about_action);
        app.set_accels_for_action("app.about", &["F1"]);

        // Dropping ISO files or game folders anywhere on the window adds them.
        let drop_target = gtk::DropTarget::new(
            gtk::gdk::FileList::static_type(),
            gtk::gdk::DragAction::COPY,
        );
        let drop_sender = sender.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            let Ok(files) = value.get::<gtk::gdk::FileList>() else {
                return false;
            };
            let paths: Vec<PathBuf> = files
                .files()
                .into_iter()
                .filter_map(|file| file.path())
                .collect();
            if paths.is_empty() {
                return false;
            }
            drop_sender.input(AppMsg::FilesChosen(paths));
            true
        });
        root.add_controller(drop_target);

        if !pending.is_empty() {
            task::inspect(sender, pending);
        }

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>) {
        match message {
            AppMsg::AddRequested => {
                if self.converting {
                    return;
                }
                self.open_iso_dialog(&sender);
            }

            AppMsg::AddFolderRequested => {
                if self.converting {
                    return;
                }
                self.open_folder_dialog(&sender);
            }

            AppMsg::FilesChosen(paths) => {
                if self.converting {
                    self.toast("Wait for the current conversion to finish.");
                    return;
                }

                let mut known: HashSet<PathBuf> = self
                    .entries
                    .iter()
                    .filter(|entry| !matches!(entry.state, EntryState::Failed(_)))
                    .map(|entry| entry.path.clone())
                    .collect();

                let mut added = Vec::new();
                for path in paths {
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    if !(path.is_file() || path.is_dir()) || !known.insert(path.clone()) {
                        continue;
                    }

                    // Re-adding a file that failed inspection retries it.
                    if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
                        self.entries.remove(index);
                    }

                    self.entries.push(IsoEntry::new(path.clone()));
                    added.push(path);
                }

                if !added.is_empty() {
                    self.reset_run_summary();
                    task::inspect(sender.clone(), added);
                }
            }

            AppMsg::Inspected(path, result) => {
                let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) else {
                    return;
                };

                match result {
                    Ok(info) => {
                        entry.info = Some(*info);
                        entry.state = EntryState::Ready;
                    }
                    Err(message) => entry.state = EntryState::Failed(message),
                }
            }

            AppMsg::Remove(index) => {
                if self.converting {
                    return;
                }
                if index < self.entries.len() {
                    self.entries.remove(index);
                    self.reset_run_summary();
                }
            }

            AppMsg::Clear => {
                if self.converting {
                    return;
                }
                self.entries.clear();
                self.reset_run_summary();
            }

            AppMsg::ChooseOutput => {
                if self.converting {
                    return;
                }
                self.open_output_dialog(&sender);
            }

            AppMsg::OutputChosen(path) => {
                self.output_dir = Some(path);
            }

            AppMsg::Convert => {
                if self.converting {
                    return;
                }

                let Some(dest_dir) = self.output_dir.clone() else {
                    self.toast("Choose an output folder first.");
                    return;
                };

                if self
                    .entries
                    .iter()
                    .any(|entry| matches!(entry.state, EntryState::Inspecting))
                {
                    self.toast("Still reading game details…");
                    return;
                }

                if self.ftp.enabled && !self.ftp.is_configured() {
                    self.toast("Enter the Xbox 360 address in Settings.");
                    return;
                }

                let jobs: Vec<Job> = self
                    .entries
                    .iter()
                    .filter_map(|entry| {
                        entry.info.as_ref().map(|info| Job {
                            path: entry.path.clone(),
                            info: info.clone(),
                        })
                    })
                    .collect();

                if jobs.is_empty() {
                    self.toast("Add at least one game.");
                    return;
                }

                if let Err(error) = std::fs::create_dir_all(&dest_dir) {
                    self.toast(&format!("Cannot create output folder: {error}"));
                    return;
                }

                self.converting = true;
                self.cancelling = false;
                self.finished = false;
                self.completed = 0;
                self.failed = 0;
                self.uploaded = 0;
                self.run_uploads = self.ftp.enabled && self.ftp.is_configured();
                self.active_job = jobs.iter().map(|job| job.path.clone()).collect();
                self.cancel = CancelToken::new();

                for entry in &mut self.entries {
                    if entry.info.is_some() {
                        entry.state = EntryState::Queued;
                    }
                }

                task::convert_all(
                    sender.clone(),
                    jobs,
                    dest_dir,
                    self.cancel.clone(),
                    iso2god_next::default_num_threads(),
                    self.ftp.enabled.then(|| self.ftp.clone()),
                );
            }

            AppMsg::Cancel => {
                if self.converting {
                    self.cancelling = true;
                    self.cancel.cancel();
                }
            }

            AppMsg::Progress(path, progress) => {
                if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
                    entry.state = EntryState::Converting(progress);
                }
            }

            AppMsg::UploadStarted(path) => {
                if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
                    entry.state = EntryState::Uploading(UploadProgress {
                        done: 0,
                        total: 0,
                        files_done: 0,
                        files_total: 0,
                        file: String::new(),
                        bytes_per_sec: None,
                    });
                }
            }

            AppMsg::UploadProgress(path, progress) => {
                if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
                    entry.state = EntryState::Uploading(progress);
                }
            }

            AppMsg::FileDone(path, outcome) => {
                let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) else {
                    return;
                };

                match outcome {
                    FileOutcome::Done { uploaded } => {
                        entry.state = EntryState::Done { uploaded };
                        self.completed += 1;
                        if uploaded {
                            self.uploaded += 1;
                        }
                    }
                    FileOutcome::Cancelled => entry.state = EntryState::Cancelled,
                    FileOutcome::Failed(message) => {
                        entry.state = EntryState::Failed(message);
                        self.failed += 1;
                    }
                }
            }

            AppMsg::AllDone => {
                for entry in &mut self.entries {
                    if matches!(
                        entry.state,
                        EntryState::Queued | EntryState::Converting(_) | EntryState::Uploading(_)
                    ) {
                        entry.state = EntryState::Cancelled;
                    }
                }

                self.converting = false;
                self.cancelling = false;
                self.finished = true;
                self.last_run_cancelled = self.cancel.is_cancelled();

                if self.cancel.is_cancelled() {
                    self.toast("Conversion cancelled");
                } else if self.failed > 0 {
                    self.toast(&format!(
                        "{} of {} games converted · {} failed",
                        self.completed,
                        self.active_job.len(),
                        self.failed
                    ));
                } else if self.completed > 0 {
                    let message = if self.uploaded == self.completed {
                        format!(
                            "Converted and uploaded {} game{}",
                            self.completed,
                            if self.completed == 1 { "" } else { "s" }
                        )
                    } else {
                        format!(
                            "Converted {} game{}",
                            self.completed,
                            if self.completed == 1 { "" } else { "s" }
                        )
                    };
                    match self.output_dir.clone() {
                        Some(folder) => self.toast_with_folder(message, folder),
                        None => self.toast(&message),
                    }
                }

                self.cancel = CancelToken::new();
            }

            AppMsg::Settings => {
                let sender = sender.clone();
                settings_dialog::present(&self.window, &self.ftp, move |settings| {
                    sender.input(AppMsg::SettingsChanged(settings));
                });
            }

            AppMsg::SettingsChanged(settings) => {
                if let Err(error) = settings.save() {
                    eprintln!("iso2god-next-gui: cannot save settings: {error}");
                    self.toast(&format!("Cannot save settings: {error}"));
                }
                self.ftp = settings;
            }

            AppMsg::About => self.show_about_dialog(),
        }

        self.refresh(&sender);
    }
}

impl AppModel {
    fn convertible_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.info.is_some())
            .count()
    }

    fn can_convert(&self) -> bool {
        !self.converting
            && self.output_dir.is_some()
            && !self
                .entries
                .iter()
                .any(|entry| matches!(entry.state, EntryState::Inspecting))
            && self.entries.iter().any(|entry| entry.info.is_some())
    }

    fn reset_run_summary(&mut self) {
        self.finished = false;
        self.last_run_cancelled = false;
        self.completed = 0;
        self.failed = 0;
        self.uploaded = 0;
        self.active_job.clear();
    }

    fn convert_label(&self) -> String {
        match self.convertible_count() {
            0 | 1 => "Convert".to_owned(),
            count => format!("Convert {count} games"),
        }
    }

    fn cancel_label(&self) -> &'static str {
        if self.cancelling {
            "Cancelling…"
        } else {
            "Cancel"
        }
    }

    fn output_subtitle(&self) -> String {
        match &self.output_dir {
            Some(dir) => dir.to_string_lossy().into_owned(),
            None => "Choose where the converted games are written".to_owned(),
        }
    }

    fn empty_output_label(&self) -> String {
        match &self.output_dir {
            Some(dir) => format!("Output: {}", dir.display()),
            None => "No output folder selected".to_owned(),
        }
    }

    /// Keeps the header actions and the list rows in sync with the model.
    fn refresh(&mut self, sender: &ComponentSender<Self>) {
        if let Some(action) = &self.add_action {
            action.set_enabled(!self.converting);
        }
        if let Some(action) = &self.add_folder_action {
            action.set_enabled(!self.converting);
        }
        if let Some(clear) = &self.clear_button {
            clear.set_sensitive(!self.converting && !self.entries.is_empty());
        }
        if let Some(action) = &self.convert_action {
            action.set_enabled(self.can_convert());
        }
        if let Some(action) = &self.cancel_action {
            action.set_enabled(self.converting && !self.cancelling);
        }

        self.sync_rows(sender);
    }

    /// Progress over the games of the current (or last) run.
    fn overall_fraction(&self) -> f64 {
        if self.active_job.is_empty() {
            return 0.0;
        }

        let mut accumulated = 0.0;
        // When uploads are part of this run, reserve the last quarter of each
        // entry for the transfer so the overall bar does not jump backwards
        // when the conversion finishes.
        let conversion_weight = if self.run_uploads { 0.75 } else { 1.0 };

        for entry in &self.entries {
            if !self.active_job.contains(&entry.path) {
                continue;
            }
            accumulated += match &entry.state {
                EntryState::Done { .. } | EntryState::Cancelled | EntryState::Failed(_) => 1.0,
                EntryState::Converting(progress) => progress.overall_fraction() * conversion_weight,
                EntryState::Uploading(progress) => {
                    conversion_weight + progress.fraction() * (1.0 - conversion_weight)
                }
                EntryState::Inspecting | EntryState::Ready | EntryState::Queued => 0.0,
            };
        }

        (accumulated / self.active_job.len() as f64).clamp(0.0, 1.0)
    }

    fn status_text(&self) -> String {
        let job_len = self.active_job.len();

        if self.converting {
            let done = self
                .entries
                .iter()
                .filter(|entry| self.active_job.contains(&entry.path) && entry.is_terminal())
                .count();
            let uploading = self
                .entries
                .iter()
                .any(|entry| matches!(entry.state, EntryState::Uploading(_)));
            return format!(
                "{} {} of {} · {}%",
                if uploading { "Uploading" } else { "Converting" },
                (done + 1).min(job_len.max(1)),
                job_len,
                format::percent(self.overall_fraction())
            );
        }

        if self.finished && job_len > 0 {
            if self.last_run_cancelled {
                return "Conversion cancelled".to_owned();
            }
            return if self.failed > 0 {
                format!(
                    "Finished · {} converted, {} failed",
                    self.completed, self.failed
                )
            } else {
                format!("Converted {} of {}", self.completed, job_len)
            };
        }

        if self.entries.is_empty() {
            return "No games added".to_owned();
        }

        let count = self.entries.len();
        let total: u64 = self
            .entries
            .iter()
            .filter_map(|entry| entry.info.as_ref().map(|info| info.data_size))
            .sum();

        format!(
            "{count} game{} · {}",
            if count == 1 { "" } else { "s" },
            format::size(total)
        )
    }

    /// Rebuilds the list rows when entries were added or removed; otherwise the
    /// existing rows are updated in place so progress stays cheap.
    fn sync_rows(&self, sender: &ComponentSender<Self>) {
        let rows_match = {
            let rows = self.rows.borrow();
            rows.len() == self.entries.len()
                && rows
                    .iter()
                    .zip(self.entries.iter())
                    .all(|(row, entry)| row.path == entry.path)
        };

        if rows_match {
            for (row, entry) in self.rows.borrow().iter().zip(self.entries.iter()) {
                row.update(entry, self.converting);
            }
            return;
        }

        let mut rows = self.rows.borrow_mut();
        for row in rows.iter() {
            self.group.remove(&row.row);
        }
        rows.clear();

        for (index, entry) in self.entries.iter().enumerate() {
            let row = IsoRow::new(index, entry, sender);
            self.group.add(&row.row);
            rows.push(row);
        }
    }

    fn open_iso_dialog(&self, sender: &ComponentSender<Self>) {
        let iso_filter = gtk::FileFilter::new();
        iso_filter.set_name(Some("Xbox ISO images"));
        iso_filter.add_suffix("iso");
        iso_filter.add_suffix("ISO");

        let all_filter = gtk::FileFilter::new();
        all_filter.set_name(Some("All files"));
        all_filter.add_pattern("*");

        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&iso_filter);
        filters.append(&all_filter);

        let mut builder = gtk::FileDialog::builder()
            .title("Add ISO images")
            .accept_label("Add")
            .modal(true)
            .filters(&filters)
            .default_filter(&iso_filter);

        if let Some(dir) = self.current_source_dir() {
            builder = builder.initial_folder(&gio::File::for_path(dir));
        }
        let dialog = builder.build();

        let window = self.window.clone();
        let sender = sender.clone();
        // Keep the dialog alive for the whole operation; the native window lives
        // as long as this future does.
        relm4::spawn_local(async move {
            let Ok(files) = dialog.open_multiple_future(Some(&window)).await else {
                return;
            };

            let mut paths = Vec::new();
            for index in 0..files.n_items() {
                if let Some(path) = files
                    .item(index)
                    .and_downcast::<gio::File>()
                    .and_then(|file| file.path())
                {
                    paths.push(path);
                }
            }

            if !paths.is_empty() {
                sender.input(AppMsg::FilesChosen(paths));
            }
        });
    }

    /// Lets the user pick game folders, each holding a default.xex (or
    /// default.xbe) with the rest of the game's files.
    fn open_folder_dialog(&self, sender: &ComponentSender<Self>) {
        let mut builder = gtk::FileDialog::builder()
            .title("Add XEX folders")
            .accept_label("Add")
            .modal(true);

        if let Some(dir) = self.current_source_dir() {
            builder = builder.initial_folder(&gio::File::for_path(dir));
        }
        let dialog = builder.build();

        let window = self.window.clone();
        let sender = sender.clone();
        relm4::spawn_local(async move {
            let Ok(folders) = dialog.select_multiple_folders_future(Some(&window)).await else {
                return;
            };

            let paths: Vec<PathBuf> = (0..folders.n_items())
                .filter_map(|index| {
                    folders
                        .item(index)
                        .and_downcast::<gio::File>()
                        .and_then(|file| file.path())
                })
                .collect();

            if !paths.is_empty() {
                sender.input(AppMsg::FilesChosen(paths));
            }
        });
    }

    /// Folder of the most recently added game, used as the picker's start folder.
    fn current_source_dir(&self) -> Option<PathBuf> {
        self.entries
            .last()
            .and_then(|entry| entry.path.parent())
            .map(Path::to_path_buf)
    }

    #[allow(deprecated)] // GtkFileDialog cannot pick folders
    fn open_output_dialog(&self, sender: &ComponentSender<Self>) {
        let dialog = gtk::FileChooserDialog::new(
            Some("Choose an output folder"),
            Some(&self.window),
            gtk::FileChooserAction::SelectFolder,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Select", gtk::ResponseType::Accept),
            ],
        );
        dialog.set_modal(true);

        if let Some(dir) = &self.output_dir {
            let _ = dialog.set_current_folder(Some(&gio::File::for_path(dir)));
        }

        let sender = sender.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept
                && let Some(path) = dialog.file().and_then(|file| file.path())
            {
                sender.input(AppMsg::OutputChosen(path));
            }
            dialog.destroy();
        });
        dialog.present();
    }

    fn show_about_dialog(&self) {
        let dialog = adw::AboutDialog::builder()
            .application_name("ISO to GOD")
            .application_icon(APP_ID)
            .developer_name("iso2god-next contributors")
            .version(env!("CARGO_PKG_VERSION"))
            .website("https://github.com/georgewoodall82/iso2god-next")
            .license_type(gtk::License::MitX11)
            .comments(
                "Convert Xbox 360 and original Xbox ISO images and game folders to the Games on Demand format.",
            )
            .build();

        dialog.present(Some(&self.window));
    }

    fn toast(&self, message: &str) {
        let toast = adw::Toast::builder().title(message).timeout(4).build();
        self.toaster.add_toast(toast);
    }

    fn toast_with_folder(&self, message: String, folder: PathBuf) {
        let toast = adw::Toast::builder()
            .title(&message)
            .button_label("Open Folder")
            .timeout(6)
            .build();
        toast.connect_button_clicked(move |toast| {
            toast.dismiss();
            let uri = gio::File::for_path(&folder).uri();
            if let Err(error) =
                gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>)
            {
                eprintln!(
                    "iso2god-next-gui: cannot open {}: {error}",
                    folder.display()
                );
            }
        });
        self.toaster.add_toast(toast);
    }
}
