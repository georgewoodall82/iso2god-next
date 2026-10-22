//! The data the GUI keeps for every ISO image or game folder in the list.

use std::path::PathBuf;

use iso2god_next::{ConversionProgress, IsoInfo, SourceKind};

use crate::ftp::UploadProgress;

/// Where an entry is in the add -> convert -> upload workflow.
#[derive(Debug)]
pub enum EntryState {
    /// Metadata is being read on a worker thread.
    Inspecting,
    /// Nothing to do yet.
    Ready,
    /// Queued up for the current conversion run.
    Queued,
    Converting(ConversionProgress),
    Uploading(UploadProgress),
    /// Converted, and optionally uploaded to the console.
    Done {
        uploaded: bool,
    },
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub struct IsoEntry {
    pub path: PathBuf,
    pub info: Option<IsoInfo>,
    pub state: EntryState,
}

impl IsoEntry {
    pub fn new(path: PathBuf) -> Self {
        IsoEntry {
            path,
            info: None,
            state: EntryState::Inspecting,
        }
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// Known game title, or the file stem (folder name) for games the title
    /// database doesn't know about.
    pub fn title(&self) -> String {
        match &self.info {
            Some(info) => info.display_name(&self.path),
            None if self.path.is_dir() => self.file_name(),
            None => self
                .path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.file_name()),
        }
    }

    pub fn is_folder(&self) -> bool {
        match &self.info {
            Some(info) => info.kind == SourceKind::Folder,
            None => self.path.is_dir(),
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            EntryState::Done { .. } | EntryState::Cancelled | EntryState::Failed(_)
        )
    }
}
