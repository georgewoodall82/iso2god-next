//! Background work: all blocking ISO parsing, conversion and FTP upload happens
//! here so the GTK main loop never waits on a spinning disk or the network.

use std::path::PathBuf;

use iso2god_next::{CancelToken, ConvertOptions, IsoInfo, TrimMode};
use relm4::ComponentSender;

use crate::app::{AppModel, AppMsg};
use crate::ftp;
use crate::settings::FtpSettings;

/// One image queued for the current run, together with the metadata read when
/// it was added (the package folder name depends on it).
#[derive(Debug, Clone)]
pub struct Job {
    pub path: PathBuf,
    pub info: IsoInfo,
}

/// How a single ISO conversion ended.
#[derive(Debug)]
pub enum FileOutcome {
    Done { uploaded: bool },
    Cancelled,
    Failed(String),
}

/// Reads metadata for every path, reporting each result as it arrives.
pub fn inspect(sender: ComponentSender<AppModel>, paths: Vec<PathBuf>) {
    std::thread::spawn(move || {
        for path in paths {
            let result = iso2god_next::inspect(&path)
                .map(Box::new)
                .map_err(|error| describe(&error));
            sender.input(AppMsg::Inspected(path, result));
        }
    });
}

/// Converts the given ISOs one at a time into `dest_dir`, then uploads each
/// finished package when FTP is enabled.
pub fn convert_all(
    sender: ComponentSender<AppModel>,
    jobs: Vec<Job>,
    dest_dir: PathBuf,
    cancel: CancelToken,
    num_threads: usize,
    ftp_settings: Option<FtpSettings>,
) {
    std::thread::spawn(move || {
        for job in jobs {
            if cancel.is_cancelled() {
                sender.input(AppMsg::FileDone(job.path, FileOutcome::Cancelled));
                continue;
            }

            let options = ConvertOptions {
                source: job.path.clone(),
                dest_dir: dest_dir.clone(),
                game_title: None,
                trim: TrimMode::FromEnd,
                num_threads,
            };

            // Only forward updates when the rounded progress actually moved;
            // a large image produces thousands of byte-level samples.
            let last_marker = std::sync::atomic::AtomicU64::new(u64::MAX);
            let progress_sender = sender.clone();
            let progress_path = job.path.clone();

            let result = iso2god_next::convert_cancellable(&options, &cancel, |progress| {
                let marker = (progress.overall_fraction() * 1000.0).round() as u64;
                if last_marker.swap(marker, std::sync::atomic::Ordering::Relaxed) != marker {
                    progress_sender.input(AppMsg::Progress(progress_path.clone(), progress));
                }
            });

            let outcome = match result {
                Ok(()) => {
                    upload_if_enabled(&sender, &job, &dest_dir, &cancel, ftp_settings.as_ref())
                }
                Err(error) => {
                    if cancel.is_cancelled() || error.is::<iso2god_next::Cancelled>() {
                        FileOutcome::Cancelled
                    } else {
                        eprintln!("iso2god-next-gui: {}: {error:#}", job.path.display());
                        FileOutcome::Failed(describe(&error))
                    }
                }
            };

            let cancelled = matches!(outcome, FileOutcome::Cancelled);
            sender.input(AppMsg::FileDone(job.path, outcome));

            if cancelled {
                break;
            }
        }

        sender.input(AppMsg::AllDone);
    });
}

/// Uploads a freshly converted package, returning the outcome of the whole
/// conversion-and-upload step.
fn upload_if_enabled(
    sender: &ComponentSender<AppModel>,
    job: &Job,
    dest_dir: &std::path::Path,
    cancel: &CancelToken,
    settings: Option<&FtpSettings>,
) -> FileOutcome {
    let Some(settings) = settings else {
        return FileOutcome::Done { uploaded: false };
    };

    if !settings.enabled || !settings.is_configured() {
        return FileOutcome::Done { uploaded: false };
    }

    if cancel.is_cancelled() {
        return FileOutcome::Cancelled;
    }

    sender.input(AppMsg::UploadStarted(job.path.clone()));

    let package_dir = job.info.package_dir_path(dest_dir);
    let relative = job.info.package_relative_path();

    let mut last_marker = u64::MAX;
    let mut last_sent = std::time::Instant::now();
    let mut last_notice: Option<String> = None;
    let progress_sender = sender.clone();
    let progress_path = job.path.clone();

    let result = ftp::upload_package(settings, &package_dir, &relative, cancel, |progress| {
        // Report at most once per tenth of a percent (and once per file), plus
        // twice a second so the speed readout keeps moving on a slow link.
        let marker =
            (progress.fraction() * 1000.0).round() as u64 + progress.files_done as u64 * 100_000;
        let stale = last_sent.elapsed() >= std::time::Duration::from_millis(500);
        // A changed notice (the retry countdown) is always shown straight away.
        let notice_changed = progress.notice != last_notice;
        if marker != last_marker || stale || notice_changed {
            last_marker = marker;
            last_notice.clone_from(&progress.notice);
            last_sent = std::time::Instant::now();
            progress_sender.input(AppMsg::UploadProgress(progress_path.clone(), progress));
        }
    });

    match result {
        Ok(()) => FileOutcome::Done { uploaded: true },
        Err(error) if cancel.is_cancelled() || error.is::<iso2god_next::Cancelled>() => {
            FileOutcome::Cancelled
        }
        Err(error) => {
            eprintln!(
                "iso2god-next-gui: upload {} -> {}: {error:#}",
                package_dir.display(),
                relative.display()
            );
            FileOutcome::Failed(format!("Converted, but the upload failed: {error:#}"))
        }
    }
}

/// One line, with the context chain but without the `anyhow` backtrace noise.
fn describe(error: &anyhow::Error) -> String {
    format!("{error:#}")
}
