//! Uploading a converted Games-On-Demand package to an Xbox 360 over FTP.
//!
//! The console runs a plain FTP server (Aurora, Freestyle Dash, XeXMenu, …) that
//! exposes its drives with backslash-separated paths such as
//! `Hdd1:\Content\0000000000000000`. Uploads happen on the conversion worker
//! thread and report byte progress, so the window can show a second progress
//! phase after the conversion itself is finished.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::ToSocketAddrs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Error, bail};
use iso2god_next::{CancelToken, Cancelled};
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpStream, ImplFtpStream, TlsStream, TransferStream};

use crate::settings::FtpSettings;

/// Connect timeout; the console is on the local network, so waiting longer than
/// this almost always means the address is wrong or the server is off.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Give up when the console stops answering for this long, so a dropped Wi-Fi
/// link cannot leave the conversion worker stuck forever.
const IO_TIMEOUT: Duration = Duration::from_secs(60);
/// A data write that blocks this long means the console has stopped reading.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the console's reply when a transfer is being dropped.
const ABORT_TIMEOUT: Duration = Duration::from_secs(5);
const COPY_BUFFER_SIZE: usize = 128 * 1024;

/// Progress of one package upload.
#[derive(Clone, Debug)]
pub struct UploadProgress {
    /// Bytes sent across all files of the package.
    pub done: u64,
    pub total: u64,
    pub files_done: usize,
    pub files_total: usize,
    /// File currently being sent, for the row subtitle.
    pub file: String,
    /// Smoothed upload speed, once enough time has passed to measure one.
    pub bytes_per_sec: Option<f64>,
    /// Set while the upload is waiting to reconnect after the console stalled.
    pub notice: Option<String>,
}

impl UploadProgress {
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
    }
}

/// Measures throughput over the whole upload. Single samples are noisy (the
/// socket buffer swallows bursts), so each half-second window is blended into a
/// running average.
struct SpeedMeter {
    window_start: Instant,
    window_done: u64,
    rate: Option<f64>,
}

impl SpeedMeter {
    const WINDOW: Duration = Duration::from_millis(500);
    /// Weight of the newest window in the running average.
    const SMOOTHING: f64 = 0.3;

    fn new(now: Instant, done: u64) -> Self {
        SpeedMeter {
            window_start: now,
            window_done: done,
            rate: None,
        }
    }

    /// Records the cumulative byte count and returns the current speed.
    fn record(&mut self, now: Instant, done: u64) -> Option<f64> {
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed >= Self::WINDOW {
            let window_rate = done.saturating_sub(self.window_done) as f64 / elapsed.as_secs_f64();
            self.rate = Some(match self.rate {
                Some(rate) => rate + (window_rate - rate) * Self::SMOOTHING,
                None => window_rate,
            });
            self.window_start = now;
            self.window_done = done;
        }
        self.rate
    }
}

/// One file of the package: full local path, path relative to the package root
/// and size in bytes.
struct PackageFile {
    local: PathBuf,
    relative: PathBuf,
    size: u64,
}

/// A remote folder as the console understands it, plus any components that
/// still have to be created.
struct RemoteDir {
    /// Path the server accepted with `CWD`, using its preferred separators.
    path: String,
    /// Components below `path` that do not exist yet.
    missing: Vec<String>,
}

impl RemoteDir {
    fn full_path(&self, fallback: &str) -> String {
        if self.path.is_empty() {
            return fallback.to_owned();
        }
        if self.missing.is_empty() {
            return self.path.clone();
        }
        let separator = if self.path.contains('/') && !self.path.contains('\\') {
            "/"
        } else {
            "\\"
        };
        format!("{}{separator}{}", self.path, self.missing.join(separator))
    }
}

/// Opens a control connection to the console and configures sane timeouts.
fn connect(settings: &FtpSettings) -> Result<FtpStream, Error> {
    let (host, port) = settings.server();
    let address = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {host}"))?
        .next()
        .with_context(|| format!("{host} did not resolve to an address"))?;

    let mut ftp = FtpStream::connect_timeout(address, CONNECT_TIMEOUT)
        .with_context(|| format!("cannot connect to the Xbox at {host}:{port}"))?;
    let _ = ftp.get_ref().set_read_timeout(Some(IO_TIMEOUT));
    let _ = ftp.get_ref().set_write_timeout(Some(IO_TIMEOUT));

    // Many console FTP servers report 0.0.0.0 in their PASV reply; fall back to
    // the control connection's peer address for the data connection.
    ftp.set_passive_nat_workaround(true);

    Ok(ftp)
}

/// Connects, logs in and looks for the content folder, without transferring
/// anything. Used by the "Test connection" button in the settings dialog.
pub fn test_connection(settings: &FtpSettings) -> Result<String, Error> {
    let (host, _) = settings.server();
    let mut ftp = connect(settings)?;

    let result = (|| -> Result<String, Error> {
        ftp.login(settings.username.as_str(), settings.password.as_str())
            .with_context(|| format!("the Xbox rejected user '{}'", settings.username))?;

        let resolved = resolve_remote_dir(&mut ftp, settings.remote_dir())?;
        if resolved.missing.is_empty() {
            Ok(format!(
                "Connected to {host} as {}; found {}",
                settings.username, resolved.path
            ))
        } else if resolved.path.is_empty() {
            Ok(format!(
                "Connected to {host} as {}; couldn't find {} - check the content folder path",
                settings.username,
                settings.remote_dir()
            ))
        } else {
            Ok(format!(
                "Connected to {host} as {}; {} will be created when uploading",
                settings.username,
                resolved.full_path(settings.remote_dir())
            ))
        }
    })();

    let _ = ftp.quit();
    result
}

/// Splits the configured folder into path components, accepting either slash.
fn path_components(configured: &str) -> Result<Vec<String>, Error> {
    let mut components = Vec::new();
    for part in configured.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => bail!("the content folder must not contain '..'"),
            part => components.push(part.to_owned()),
        }
    }
    Ok(components)
}

/// Candidate spellings of a path: both separators, with and without a leading
/// separator, and with and without the drive colon. Xbox 360 FTP servers differ
/// in which one they accept for `CWD`.
fn path_candidates(components: &[String]) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();

    let mut sets = vec![components.to_vec()];
    if components.len() > 1 && components[0].ends_with(':') {
        // Some servers are already rooted at the drive, so `Content\...` works.
        sets.push(components[1..].to_vec());
    }

    for set in sets {
        for colon in [true, false] {
            let mut parts = set.clone();
            if !colon && let Some(first) = parts.first_mut() {
                *first = first.trim_end_matches(':').to_owned();
                if first.is_empty() {
                    continue;
                }
            }

            for separator in ['\\', '/'] {
                let joined = parts.join(&separator.to_string());
                for candidate in [joined.clone(), format!("{separator}{joined}")] {
                    if !candidates.contains(&candidate) {
                        candidates.push(candidate);
                    }
                }
            }
        }
    }

    candidates
}

/// Finds the deepest part of the configured folder that the console can open.
fn resolve_remote_dir(ftp: &mut FtpStream, configured: &str) -> Result<RemoteDir, Error> {
    let components = path_components(configured)?;
    if components.is_empty() {
        return Ok(RemoteDir {
            path: configured.trim().to_owned(),
            missing: Vec::new(),
        });
    }

    for candidate in path_candidates(&components) {
        if ftp.cwd(&candidate).is_ok() {
            return Ok(RemoteDir {
                path: candidate,
                missing: Vec::new(),
            });
        }
    }

    // The full folder is missing (or spelled differently). Find the longest
    // prefix that exists so only the missing levels have to be created.
    for take in (1..components.len()).rev() {
        for candidate in path_candidates(&components[..take]) {
            if ftp.cwd(&candidate).is_ok() {
                return Ok(RemoteDir {
                    path: candidate,
                    missing: components[take..].to_vec(),
                });
            }
        }
    }

    Ok(RemoteDir {
        path: String::new(),
        missing: components,
    })
}

/// Creates the missing components of `dir` and returns the full remote path.
fn ensure_remote_dir(ftp: &mut FtpStream, mut dir: RemoteDir) -> Result<String, Error> {
    let separator = if dir.path.contains('/') && !dir.path.contains('\\') {
        "/"
    } else {
        "\\"
    };

    if dir.path.is_empty() && ftp.cwd("/").is_err() && ftp.cwd("\\").is_err() {
        bail!("cannot return to the root of the Xbox FTP server");
    }

    for component in &dir.missing {
        let actual = enter_or_create(ftp, component)?;
        if dir.path.is_empty() {
            dir.path = actual;
        } else {
            dir.path.push_str(separator);
            dir.path.push_str(&actual);
        }
    }

    Ok(dir.path)
}

/// Enters a directory relative to the current one, creating it if needed, and
/// reports the spelling the server accepted.
fn enter_or_create(ftp: &mut FtpStream, component: &str) -> Result<String, Error> {
    if ftp.cwd(component).is_ok() {
        return Ok(component.to_owned());
    }

    // A drive may be exposed as `Hdd1:` or `Hdd1` depending on the server.
    let alternative = component.trim_end_matches(':');
    if alternative != component && ftp.cwd(alternative).is_ok() {
        return Ok(alternative.to_owned());
    }

    // `mkdir` fails with 550 when the folder already exists, which is fine;
    // the following `cwd` decides whether it is usable.
    let _ = ftp.mkdir(component);
    if ftp.cwd(component).is_ok() {
        return Ok(component.to_owned());
    }

    if alternative != component && !alternative.is_empty() {
        let _ = ftp.mkdir(alternative);
        if ftp.cwd(alternative).is_ok() {
            return Ok(alternative.to_owned());
        }
    }

    bail!("cannot create the folder {component} on the Xbox")
}

/// A file transfer that may be retried: where the upload stands, so a fresh
/// connection can carry on from there.
struct Session {
    /// Index of the file being sent (all earlier files are complete).
    file: usize,
    /// Bytes of that file already written to the console.
    sent: u64,
    /// Whether any connection has got as far as the content folder. Until then a
    /// failure is a configuration problem (wrong address, wrong password), not a
    /// stall, and is reported at once instead of retried.
    connected: bool,
    /// Cleared when the console resumed a file incorrectly, so the retry sends
    /// that file again from the start and never trusts `REST` again.
    resume_allowed: bool,
}

impl Session {
    fn completed_bytes(&self, files: &[PackageFile]) -> u64 {
        files[..self.file.min(files.len())]
            .iter()
            .map(|file| file.size)
            .sum()
    }

    fn done(&self, files: &[PackageFile]) -> u64 {
        self.completed_bytes(files) + self.sent
    }

    fn progress(
        &self,
        files: &[PackageFile],
        total: u64,
        bytes_per_sec: Option<f64>,
        notice: Option<String>,
    ) -> UploadProgress {
        UploadProgress {
            done: self.done(files),
            total,
            files_done: self.file,
            files_total: files.len(),
            file: files
                .get(self.file)
                .and_then(|file| file.relative.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            bytes_per_sec,
            notice,
        }
    }

    /// A failure while connecting or logging in: fatal on the very first
    /// connection, a stall afterwards.
    fn setup_failure(&self, error: Error) -> Failure {
        if self.connected {
            Failure::Retry(error)
        } else {
            Failure::Fatal(error)
        }
    }
}

/// How an upload attempt ended badly.
enum Failure {
    /// Retrying cannot help (cancelled, bad settings, unreadable local file).
    Fatal(Error),
    /// The connection broke or the console stopped answering; try again.
    Retry(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Failure::Retry(error)
    }
}

/// How hard to try again when the console stalls or drops the connection.
#[derive(Clone, Copy)]
struct RetryPolicy {
    /// Give up after this many attempts in a row that moved no data forward.
    max_failed_attempts: u32,
    first_delay: Duration,
    max_delay: Duration,
}

const RETRY_POLICY: RetryPolicy = RetryPolicy {
    max_failed_attempts: 8,
    first_delay: Duration::from_secs(5),
    max_delay: Duration::from_secs(30),
};

/// Uploads the contents of `package_dir` (a `<TitleID>/<ContentType>` folder)
/// into `remote_relative` below the configured content folder on the console.
///
/// Consoles sometimes stall for minutes in the middle of a large transfer. When
/// that happens the upload reconnects and carries on with the current file
/// (resuming it when the server supports that) instead of failing outright.
pub fn upload_package<F>(
    settings: &FtpSettings,
    package_dir: &Path,
    remote_relative: &Path,
    cancel: &CancelToken,
    on_progress: F,
) -> Result<(), Error>
where
    F: FnMut(UploadProgress),
{
    upload_package_with(
        RETRY_POLICY,
        settings,
        package_dir,
        remote_relative,
        cancel,
        on_progress,
    )
}

fn upload_package_with<F>(
    policy: RetryPolicy,
    settings: &FtpSettings,
    package_dir: &Path,
    remote_relative: &Path,
    cancel: &CancelToken,
    mut on_progress: F,
) -> Result<(), Error>
where
    F: FnMut(UploadProgress),
{
    let files = collect_files(package_dir)
        .with_context(|| format!("cannot read {}", package_dir.display()))?;
    if files.is_empty() {
        bail!("the converted package is empty");
    }

    let total: u64 = files.iter().map(|file| file.size).sum();
    let mut session = Session {
        file: 0,
        sent: 0,
        connected: false,
        resume_allowed: true,
    };
    let mut failed_attempts = 0u32;

    loop {
        let done_before = session.done(&files);
        let error = match upload_attempt(
            settings,
            &files,
            remote_relative,
            total,
            cancel,
            &mut session,
            &mut on_progress,
        ) {
            Ok(()) => return Ok(()),
            Err(Failure::Fatal(error)) => return Err(error),
            Err(Failure::Retry(error)) => error,
        };

        if cancel.is_cancelled() || error.is::<Cancelled>() {
            return Err(Cancelled.into());
        }

        if session.done(&files) > done_before {
            failed_attempts = 0;
        }
        failed_attempts += 1;
        if failed_attempts >= policy.max_failed_attempts {
            return Err(error.context(format!(
                "the Xbox stopped responding and {failed_attempts} attempts to reconnect failed"
            )));
        }

        eprintln!(
            "iso2god-next-gui: upload interrupted ({error:#}); reconnecting \
             (attempt {failed_attempts} of {})",
            policy.max_failed_attempts
        );
        wait_to_retry(
            policy,
            failed_attempts,
            cancel,
            &session,
            &files,
            total,
            &mut on_progress,
        )?;
    }
}

/// Sleeps before the next attempt, telling the window why nothing is moving.
fn wait_to_retry<F>(
    policy: RetryPolicy,
    failed_attempts: u32,
    cancel: &CancelToken,
    session: &Session,
    files: &[PackageFile],
    total: u64,
    on_progress: &mut F,
) -> Result<(), Error>
where
    F: FnMut(UploadProgress),
{
    let delay = policy
        .first_delay
        .saturating_mul(1 << (failed_attempts - 1).min(16))
        .min(policy.max_delay);
    let deadline = Instant::now() + delay;
    let mut last_notice = None;

    loop {
        if cancel.is_cancelled() {
            bail!(Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }

        let seconds = remaining.as_secs() + 1;
        if last_notice != Some(seconds) {
            last_notice = Some(seconds);
            on_progress(session.progress(
                files,
                total,
                None,
                Some(format!(
                    "The Xbox stopped responding · reconnecting in {seconds}s \
                     (attempt {} of {})",
                    failed_attempts + 1,
                    policy.max_failed_attempts
                )),
            ));
        }
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

/// Where a retried file should restart. Returns how many bytes the console
/// already holds (to resume after), the whole size when it has everything, or
/// 0 to send the file again from the start.
fn resume_point(ftp: &mut FtpStream, name: &str, sent: u64) -> u64 {
    match ftp.size(name) {
        Ok(remote) if remote > 0 && remote as u64 <= sent => remote as u64,
        _ => 0,
    }
}

/// Opens the data connection for `name`, resuming at `start` when the server
/// allows it. Returns the stream and the offset the transfer really starts at.
fn open_upload<T: TlsStream>(
    ftp: &mut ImplFtpStream<T>,
    name: &str,
    start: u64,
) -> Result<(TransferStream<T>, u64), Error> {
    if start > 0 {
        let _ = ftp.resume_transfer(start as usize);
        match ftp.put_with_stream(name) {
            Ok(stream) => return Ok((stream, start)),
            Err(FtpError::UnexpectedResponse(_)) => {
                // The server refused REST; start the file over instead.
                let _ = ftp.resume_transfer(0);
            }
            Err(error) => return Err(error).with_context(|| format!("cannot create {name}")),
        }
    }

    let stream = ftp
        .put_with_stream(name)
        .with_context(|| format!("cannot create {name} on the Xbox"))?;
    Ok((stream, 0))
}

/// One connection's worth of uploading, starting from `session`.
fn upload_attempt<F>(
    settings: &FtpSettings,
    files: &[PackageFile],
    remote_relative: &Path,
    total: u64,
    cancel: &CancelToken,
    session: &mut Session,
    on_progress: &mut F,
) -> Result<(), Failure>
where
    F: FnMut(UploadProgress),
{
    let mut ftp = connect(settings).map_err(|error| session.setup_failure(error))?;

    // Find how this server spells the content folder and create whatever is
    // missing; the package goes in below it.
    let setup = (|| -> Result<String, Error> {
        ftp.login(settings.username.as_str(), settings.password.as_str())
            .with_context(|| format!("the Xbox rejected user '{}'", settings.username))?;
        ftp.transfer_type(FileType::Binary)
            .context("the Xbox does not accept binary transfers")?;
        let content_root = resolve_remote_dir(&mut ftp, settings.remote_dir())?;
        ensure_remote_dir(&mut ftp, content_root)
    })();
    let content_root = match setup {
        Ok(content_root) => content_root,
        Err(error) => return Err(session.setup_failure(error)),
    };
    session.connected = true;

    let result = send_files(
        &mut ftp,
        &content_root,
        files,
        remote_relative,
        total,
        cancel,
        session,
        on_progress,
    );

    // A broken connection would make QUIT wait out the timeout for nothing.
    if result.is_ok() {
        let _ = ftp.quit();
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn send_files<F>(
    ftp: &mut FtpStream,
    content_root: &str,
    files: &[PackageFile],
    remote_relative: &Path,
    total: u64,
    cancel: &CancelToken,
    session: &mut Session,
    on_progress: &mut F,
) -> Result<(), Failure>
where
    F: FnMut(UploadProgress),
{
    let mut meter: Option<SpeedMeter> = None;
    let mut bytes_per_sec = None;

    while session.file < files.len() {
        let index = session.file;
        let file = &files[index];

        if cancel.is_cancelled() {
            return Err(Failure::Fatal(Cancelled.into()));
        }

        let relative_dir = file.relative.parent().unwrap_or(Path::new(""));
        let relative_to_root = remote_relative.join(relative_dir);
        let remote_dir = remote_path(content_root, &relative_to_root);
        let depth = enter_relative_dir(ftp, &relative_to_root)
            .with_context(|| format!("cannot create {remote_dir} on the Xbox"))?;
        let remote_name = file
            .relative
            .file_name()
            .and_then(|name| name.to_str())
            .context("the package contains a file name that is not valid UTF-8")
            .map_err(Failure::Fatal)?;

        // A file that was interrupted halfway: see how much of it arrived.
        if session.sent > 0 {
            session.sent = if session.resume_allowed {
                resume_point(ftp, remote_name, session.sent)
            } else {
                0
            };
        }
        on_progress(session.progress(files, total, bytes_per_sec, None));

        if session.sent < file.size || file.size == 0 {
            let mut source = File::open(&file.local)
                .with_context(|| format!("cannot open {}", file.local.display()))
                .map_err(Failure::Fatal)?;

            // Kept to cut the wait short when the transfer has to be dropped.
            let control = ftp.get_ref().try_clone().ok();
            let (mut remote, start) = open_upload(ftp, remote_name, session.sent)?;
            session.sent = start;
            source
                .seek(SeekFrom::Start(start))
                .with_context(|| format!("cannot read {}", file.local.display()))
                .map_err(Failure::Fatal)?;
            let _ = remote
                .get_ref()
                .get_ref()
                .set_write_timeout(Some(STALL_TIMEOUT));

            let give_up = |remote| {
                // Dropping the stream waits for the console's reply on the
                // control connection; do not wait a minute for a dead console.
                if let Some(control) = &control {
                    let _ = control.set_read_timeout(Some(ABORT_TIMEOUT));
                }
                drop(remote);
            };

            let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
            loop {
                if cancel.is_cancelled() {
                    give_up(remote);
                    return Err(Failure::Fatal(Cancelled.into()));
                }

                let read = source
                    .read(&mut buffer)
                    .with_context(|| format!("cannot read {}", file.local.display()))
                    .map_err(Failure::Fatal)?;
                if read == 0 {
                    break;
                }

                let done_before = session.done(files);
                if let Err(error) = remote.write_all(&buffer[..read]) {
                    give_up(remote);
                    return Err(Failure::Retry(
                        Error::new(error).context(format!("error uploading {remote_name}")),
                    ));
                }
                session.sent += read as u64;

                bytes_per_sec = meter
                    .get_or_insert_with(|| SpeedMeter::new(Instant::now(), done_before))
                    .record(Instant::now(), session.done(files));
                on_progress(session.progress(files, total, bytes_per_sec, None));
            }

            remote
                .finish()
                .with_context(|| format!("the Xbox did not finish receiving {remote_name}"))?;

            // A resumed file is only trusted if the console now holds exactly
            // what we have: a server that ignores REST would have started the
            // file over and left a corrupt one behind.
            if start > 0 && !matches!(ftp.size(remote_name), Ok(size) if size as u64 == file.size) {
                session.sent = 0;
                session.resume_allowed = false;
                return Err(Failure::Retry(anyhow::anyhow!(
                    "the Xbox did not resume {remote_name} correctly; sending it again"
                )));
            }
        }

        // Return to the content folder so the next file starts from the
        // same place even when the server only accepts relative paths.
        for _ in 0..depth {
            ftp.cdup()
                .context("cannot return to the content folder on the Xbox")?;
        }

        session.file += 1;
        session.sent = 0;
    }

    on_progress(UploadProgress {
        done: total,
        total,
        files_done: files.len(),
        files_total: files.len(),
        file: String::new(),
        bytes_per_sec,
        notice: None,
    });

    Ok(())
}

/// Walks down from the resolved content folder, creating the package folders,
/// and returns how many levels were entered. Callers use that to `CDUP` back to
/// the content folder; this works whether the server only accepts relative
/// paths or absolute ones.
fn enter_relative_dir(ftp: &mut FtpStream, relative: &Path) -> Result<usize, Error> {
    let mut depth = 0;

    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = name
            .to_str()
            .context("the package contains a folder name that is not valid UTF-8")?;
        if name == ".." {
            bail!("refusing to upload outside the content folder");
        }
        enter_or_create(ftp, name)?;
        depth += 1;
    }

    Ok(depth)
}

/// Joins a remote base with a local relative path, matching the base's
/// separator style.
pub fn remote_path(base: &str, relative: &Path) -> String {
    let separator = if base.contains('/') && !base.contains('\\') {
        '/'
    } else {
        '\\'
    };
    let relative = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(&separator.to_string());

    if relative.is_empty() {
        base.to_owned()
    } else {
        format!("{base}{separator}{relative}")
    }
}

/// Lists every regular file below `root`, sorted by path so uploads are
/// deterministic (header before data does not matter, but it helps debugging).
fn collect_files(root: &Path) -> io::Result<Vec<PackageFile>> {
    fn walk(root: &Path, dir: &Path, files: &mut Vec<PackageFile>) -> io::Result<()> {
        let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                walk(root, &path, files)?;
            } else if file_type.is_file() {
                let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                files.push(PackageFile {
                    size: entry.metadata()?.len(),
                    local: path,
                    relative,
                });
            }
        }

        Ok(())
    }

    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A tiny FTP server that records commands and uploaded files.
    struct MockServer {
        port: u16,
        commands: Arc<Mutex<Vec<String>>>,
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    /// How the mock reacts to `REST` before a `STOR`.
    #[derive(Clone, Copy, PartialEq)]
    enum RestMode {
        /// Keeps the first `offset` bytes and appends the rest.
        Honor,
        /// Answers 502, like a server without resume support.
        Refuse,
        /// Accepts the command but overwrites the file from the start.
        Ignore,
    }

    #[derive(Clone, Copy)]
    struct Behavior {
        /// Cuts the first upload dead (data and control connection) after this
        /// many bytes, the way a stalled console looks to the client.
        drop_first_store_after: Option<usize>,
        rest: RestMode,
    }

    const WELL_BEHAVED: Behavior = Behavior {
        drop_first_store_after: None,
        rest: RestMode::Honor,
    };

    fn start_server() -> MockServer {
        start_server_with(WELL_BEHAVED)
    }

    fn start_server_with(behavior: Behavior) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let files = Arc::new(Mutex::new(HashMap::new()));

        let stores = Arc::new(AtomicUsize::new(0));

        let thread_commands = Arc::clone(&commands);
        let thread_files = Arc::clone(&files);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let commands = Arc::clone(&thread_commands);
                let files = Arc::clone(&thread_files);
                let stores = Arc::clone(&stores);
                std::thread::spawn(move || serve(stream, commands, files, stores, behavior));
            }
        });

        MockServer {
            port,
            commands,
            files,
        }
    }

    fn serve(
        stream: TcpStream,
        commands: Arc<Mutex<Vec<String>>>,
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        stores: Arc<AtomicUsize>,
        behavior: Behavior,
    ) {
        let mut rest_offset = 0usize;
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut cwd = String::new();
        let mut data_listener: Option<TcpListener> = None;

        writer.write_all(b"220 mock ready\r\n").unwrap();

        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let line = line.trim_end_matches(['\r', '\n']).to_owned();
            commands.lock().unwrap().push(line.clone());

            let (command, argument) = line.split_once(' ').unwrap_or((&line, ""));
            let reply = match command.to_ascii_uppercase().as_str() {
                "USER" => "331 password required\r\n",
                "PASS" => "230 logged in\r\n",
                "TYPE" => "200 binary\r\n",
                "CWD" => {
                    if argument.contains(':')
                        || argument.starts_with('/')
                        || argument.starts_with('\\')
                        || cwd.is_empty()
                    {
                        cwd = argument.to_owned();
                    } else {
                        cwd = format!("{cwd}\\{argument}");
                    }
                    "250 directory changed\r\n"
                }
                "CDUP" => {
                    let trimmed = cwd.trim_end_matches(['\\', '/']);
                    cwd = match trimmed.rfind(['\\', '/']) {
                        Some(index) => trimmed[..index].to_owned(),
                        None => String::new(),
                    };
                    "250 directory changed\r\n"
                }
                "MKD" => "257 created\r\n",
                "PASV" => {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    let port = listener.local_addr().unwrap().port();
                    data_listener = Some(listener);
                    let reply = format!(
                        "227 Entering Passive Mode (127,0,0,1,{},{}).\r\n",
                        port >> 8,
                        port & 0xff
                    );
                    writer.write_all(reply.as_bytes()).unwrap();
                    continue;
                }
                "SIZE" => {
                    let key = if cwd.is_empty() {
                        argument.to_owned()
                    } else {
                        format!("{cwd}\\{argument}")
                    };
                    let size = files.lock().unwrap().get(&key).map(Vec::len);
                    match size {
                        Some(size) => {
                            writer
                                .write_all(format!("213 {size}\r\n").as_bytes())
                                .unwrap();
                            continue;
                        }
                        None => "550 no such file\r\n",
                    }
                }
                "REST" => {
                    if behavior.rest == RestMode::Refuse {
                        "502 not implemented\r\n"
                    } else {
                        rest_offset = argument.parse().unwrap();
                        "350 restarting\r\n"
                    }
                }
                "STOR" => {
                    writer.write_all(b"150 send it\r\n").unwrap();
                    let (data, _) = data_listener.take().unwrap().accept().unwrap();
                    let key = if cwd.is_empty() {
                        argument.to_owned()
                    } else {
                        format!("{cwd}\\{argument}")
                    };

                    let first_store =
                        argument.starts_with("Data") && stores.fetch_add(1, Ordering::SeqCst) == 0;
                    let limit = behavior.drop_first_store_after.filter(|_| first_store);
                    let mut contents = Vec::new();
                    data.take(limit.unwrap_or(usize::MAX) as u64)
                        .read_to_end(&mut contents)
                        .unwrap();

                    let mut files = files.lock().unwrap();
                    let mut stored = match behavior.rest {
                        RestMode::Honor => {
                            let mut kept = files.get(&key).cloned().unwrap_or_default();
                            kept.truncate(rest_offset);
                            kept
                        }
                        _ => Vec::new(),
                    };
                    rest_offset = 0;
                    stored.extend_from_slice(&contents);
                    files.insert(key, stored);

                    if limit.is_some() {
                        // Vanish without a word.
                        return;
                    }
                    "226 done\r\n"
                }
                "QUIT" => {
                    writer.write_all(b"221 bye\r\n").unwrap();
                    break;
                }
                _ => "200 ok\r\n",
            };
            writer.write_all(reply.as_bytes()).unwrap();
        }
    }

    #[test]
    fn path_candidates_cover_common_xbox_spellings() {
        let components = path_components(r"Hdd1:\Content\0000000000000000").unwrap();
        let candidates = path_candidates(&components);

        assert!(candidates.contains(&r"Hdd1:\Content\0000000000000000".to_owned()));
        assert!(candidates.contains(&"Hdd1:/Content/0000000000000000".to_owned()));
        assert!(candidates.contains(&r"Content\0000000000000000".to_owned()));
        assert!(candidates.contains(&"/Hdd1/Content/0000000000000000".to_owned()));
    }

    #[test]
    fn connection_test_logs_in_without_transferring_files() {
        let server = start_server();
        let settings = FtpSettings {
            enabled: true,
            host: format!("127.0.0.1:{}", server.port),
            username: "xboxftp".to_owned(),
            password: "xboxftp".to_owned(),
            remote_dir: crate::settings::DEFAULT_REMOTE_DIR.to_owned(),
        };

        let message = test_connection(&settings).unwrap();
        assert!(message.contains("Connected to 127.0.0.1"));

        let commands = server.commands.lock().unwrap();
        assert!(commands.iter().any(|command| command == "USER xboxftp"));
        assert!(commands.iter().any(|command| command == "PASS xboxftp"));
        assert!(commands.iter().any(|command| command == "QUIT"));
        assert!(!commands.iter().any(|command| command.starts_with("STOR")));
        assert!(server.files.lock().unwrap().is_empty());
    }

    #[test]
    fn speed_meter_waits_for_a_full_window_then_smooths() {
        let start = Instant::now();
        let mut meter = SpeedMeter::new(start, 0);

        assert_eq!(
            meter.record(start + Duration::from_millis(100), 1 << 20),
            None
        );

        // 1 MiB over the first full second window.
        let first = meter
            .record(start + Duration::from_secs(1), 1 << 20)
            .expect("speed after a full window");
        assert!((first - (1 << 20) as f64).abs() < 1.0);

        // A stalled window pulls the average down, but not straight to zero.
        let second = meter
            .record(start + Duration::from_secs(2), 1 << 20)
            .unwrap();
        assert!(second > 0.0 && second < first);
    }

    fn package_with_big_file(name: &str, size: usize) -> (PathBuf, PathBuf, Vec<u8>) {
        let root = std::env::temp_dir().join(format!(
            "iso2god-next-ftp-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let package = root.join("ABCD1234").join("00007000");
        fs::create_dir_all(package.join("01020304.data")).unwrap();
        fs::write(package.join("01020304"), b"con header").unwrap();
        let data: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
        fs::write(package.join("01020304.data").join("Data0000"), &data).unwrap();
        (root, package, data)
    }

    fn mock_settings(server: &MockServer) -> FtpSettings {
        FtpSettings {
            enabled: true,
            host: format!("127.0.0.1:{}", server.port),
            username: "xboxftp".to_owned(),
            password: "xboxftp".to_owned(),
            remote_dir: crate::settings::DEFAULT_REMOTE_DIR.to_owned(),
        }
    }

    const FAST_RETRIES: RetryPolicy = RetryPolicy {
        max_failed_attempts: 4,
        first_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(20),
    };

    /// Uploads a package whose big file is cut off partway and returns what the
    /// console ended up holding, plus the progress notices that were shown.
    fn upload_through_a_stall(rest: RestMode) -> (Vec<u8>, Vec<u8>, Vec<String>, Vec<String>) {
        let (root, package, data) = package_with_big_file(
            match rest {
                RestMode::Honor => "honor",
                RestMode::Refuse => "refuse",
                RestMode::Ignore => "ignore",
            },
            900_000,
        );
        let server = start_server_with(Behavior {
            drop_first_store_after: Some(300_000),
            rest,
        });
        let relative = Path::new("ABCD1234").join("00007000");
        let remote_dir = remote_path(crate::settings::DEFAULT_REMOTE_DIR, &relative);

        let mut notices = Vec::new();
        upload_package_with(
            FAST_RETRIES,
            &mock_settings(&server),
            &package,
            &relative,
            &CancelToken::new(),
            |progress| {
                if let Some(notice) = progress.notice {
                    notices.push(notice);
                }
            },
        )
        .unwrap();

        let files = server.files.lock().unwrap();
        let header = files[&format!("{remote_dir}\\01020304")].clone();
        let uploaded = files[&format!("{remote_dir}\\01020304.data\\Data0000")].clone();
        let commands = server.commands.lock().unwrap().clone();
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(header, b"con header");
        (uploaded, data, notices, commands)
    }

    #[test]
    fn a_stalled_upload_reconnects_and_resumes_the_file() {
        let (uploaded, data, notices, commands) = upload_through_a_stall(RestMode::Honor);

        assert!(
            uploaded == data,
            "the resumed file differs from the original"
        );
        assert!(!notices.is_empty(), "the stall was not reported");
        assert!(commands.iter().any(|command| command.starts_with("REST ")));
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("STOR "))
                .count(),
            3,
            "header, the cut-off file and its resumed remainder"
        );
    }

    #[test]
    fn a_server_without_resume_gets_the_file_again_from_the_start() {
        let (uploaded, data, notices, _) = upload_through_a_stall(RestMode::Refuse);

        assert!(uploaded == data);
        assert!(!notices.is_empty());
    }

    #[test]
    fn a_server_that_ignores_resume_does_not_end_up_with_a_corrupt_file() {
        let (uploaded, data, _, commands) = upload_through_a_stall(RestMode::Ignore);

        assert!(uploaded == data, "the file was left corrupt");
        // Resumed once, found the wrong size, then sent the whole file again.
        assert!(commands.iter().any(|command| command.starts_with("REST ")));
    }

    #[test]
    fn a_wrong_address_fails_at_once_instead_of_retrying() {
        let (root, package, _) = package_with_big_file("unreachable", 1000);
        // Nothing listens on this port.
        let unused = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = unused.local_addr().unwrap().port();
        drop(unused);

        let settings = FtpSettings {
            enabled: true,
            host: format!("127.0.0.1:{port}"),
            username: "xboxftp".to_owned(),
            password: "xboxftp".to_owned(),
            remote_dir: crate::settings::DEFAULT_REMOTE_DIR.to_owned(),
        };

        let started = Instant::now();
        let error = upload_package_with(
            RetryPolicy {
                max_failed_attempts: 5,
                first_delay: Duration::from_secs(30),
                max_delay: Duration::from_secs(30),
            },
            &settings,
            &package,
            Path::new("ABCD1234/00007000"),
            &CancelToken::new(),
            |_| {},
        )
        .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(format!("{error:#}").contains("cannot connect"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cancelling_while_waiting_to_reconnect_stops_the_upload() {
        let (root, package, _) = package_with_big_file("cancel", 400_000);
        let server = start_server_with(Behavior {
            drop_first_store_after: Some(100_000),
            rest: RestMode::Honor,
        });
        let cancel = CancelToken::new();
        let canceller = cancel.clone();

        let error = upload_package_with(
            RetryPolicy {
                max_failed_attempts: 4,
                first_delay: Duration::from_secs(30),
                max_delay: Duration::from_secs(30),
            },
            &mock_settings(&server),
            &package,
            Path::new("ABCD1234/00007000"),
            &cancel,
            |progress| {
                if progress.notice.is_some() {
                    canceller.cancel();
                }
            },
        )
        .unwrap_err();

        assert!(error.is::<Cancelled>());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn uploads_every_file_into_the_remote_package_folder() {
        let root =
            std::env::temp_dir().join(format!("iso2god-next-ftp-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let package = root.join("ABCD1234").join("00007000");
        fs::create_dir_all(package.join("01020304.data")).unwrap();
        fs::write(package.join("01020304"), b"con header").unwrap();
        fs::write(
            package.join("01020304.data").join("Data0000"),
            vec![7u8; 5000],
        )
        .unwrap();

        let server = start_server();
        let settings = FtpSettings {
            enabled: true,
            host: format!("127.0.0.1:{}", server.port),
            username: "xboxftp".to_owned(),
            password: "xboxftp".to_owned(),
            remote_dir: crate::settings::DEFAULT_REMOTE_DIR.to_owned(),
        };
        let relative = Path::new("ABCD1234").join("00007000");
        let remote_dir = remote_path(crate::settings::DEFAULT_REMOTE_DIR, &relative);

        let mut last = None;
        upload_package(
            &settings,
            &package,
            &relative,
            &CancelToken::new(),
            |progress| last = Some(progress),
        )
        .unwrap();

        let last = last.expect("progress was reported");
        assert_eq!(last.files_done, 2);
        assert_eq!(last.files_total, 2);
        assert_eq!(last.done, 5010);
        assert_eq!(last.fraction(), 1.0);

        let files = server.files.lock().unwrap();
        assert_eq!(
            files
                .get(&format!("{remote_dir}\\01020304"))
                .map(Vec::as_slice),
            Some(b"con header".as_slice())
        );
        assert_eq!(
            files
                .get(&format!("{remote_dir}\\01020304.data\\Data0000"))
                .map(Vec::as_slice),
            Some([7u8; 5000].as_slice())
        );

        let commands = server.commands.lock().unwrap();
        assert!(commands.iter().any(|command| command == "TYPE I"));
        assert!(commands.iter().any(|command| command == "PASV"));

        fs::remove_dir_all(&root).unwrap();
    }
}
