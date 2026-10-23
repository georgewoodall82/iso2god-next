//! Uploading a converted Games-On-Demand package to an Xbox 360 over FTP.
//!
//! The console runs a plain FTP server (Aurora, Freestyle Dash, XeXMenu, …) that
//! exposes its drives with backslash-separated paths such as
//! `Hdd1:\Content\0000000000000000`. Uploads happen on the conversion worker
//! thread and report byte progress, so the window can show a second progress
//! phase after the conversion itself is finished.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Error, bail};
use iso2god_next::{CancelToken, Cancelled};
use suppaftp::FtpStream;
use suppaftp::types::FileType;

use crate::settings::FtpSettings;

/// Connect timeout; the console is on the local network, so waiting longer than
/// this almost always means the address is wrong or the server is off.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Give up when the console stops answering for this long, so a dropped Wi-Fi
/// link cannot leave the conversion worker stuck forever.
const IO_TIMEOUT: Duration = Duration::from_secs(60);
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

    fn new(now: Instant) -> Self {
        SpeedMeter {
            window_start: now,
            window_done: 0,
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

/// Uploads the contents of `package_dir` (a `<TitleID>/<ContentType>` folder)
/// into `remote_relative` below the configured content folder on the console.
pub fn upload_package<F>(
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
    let files_total = files.len();

    let mut ftp = connect(settings)?;

    let result = (|| -> Result<(), Error> {
        ftp.login(settings.username.as_str(), settings.password.as_str())
            .with_context(|| format!("the Xbox rejected user '{}'", settings.username))?;
        ftp.transfer_type(FileType::Binary)
            .context("the Xbox does not accept binary transfers")?;

        // Find how this server spells the content folder, create whatever is
        // missing, then upload the package below it.
        let content_root = resolve_remote_dir(&mut ftp, settings.remote_dir())?;
        let content_root = ensure_remote_dir(&mut ftp, content_root)?;

        let mut done = 0u64;
        let mut meter = SpeedMeter::new(Instant::now());
        let mut bytes_per_sec = None;
        for (index, file) in files.iter().enumerate() {
            if cancel.is_cancelled() {
                bail!(Cancelled);
            }

            let relative_dir = file.relative.parent().unwrap_or(Path::new(""));
            let relative_to_root = remote_relative.join(relative_dir);
            let remote_dir = remote_path(&content_root, &relative_to_root);
            let depth = enter_relative_dir(&mut ftp, &relative_to_root)
                .with_context(|| format!("cannot create {remote_dir} on the Xbox"))?;
            let remote_name = file
                .relative
                .file_name()
                .and_then(|name| name.to_str())
                .context("the package contains a file name that is not valid UTF-8")?;

            on_progress(UploadProgress {
                done,
                total,
                files_done: index,
                files_total,
                file: remote_name.to_owned(),
                bytes_per_sec,
            });

            let mut source = File::open(&file.local)
                .with_context(|| format!("cannot open {}", file.local.display()))?;
            let mut remote = ftp
                .put_with_stream(remote_name)
                .with_context(|| format!("cannot create {remote_name} on the Xbox"))?;
            let _ = remote
                .get_ref()
                .get_ref()
                .set_write_timeout(Some(IO_TIMEOUT));

            let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
            loop {
                if cancel.is_cancelled() {
                    // Dropping the stream finalizes (aborts) the transfer before
                    // the control connection is reused.
                    drop(remote);
                    bail!(Cancelled);
                }

                let read = source
                    .read(&mut buffer)
                    .with_context(|| format!("cannot read {}", file.local.display()))?;
                if read == 0 {
                    break;
                }

                remote
                    .write_all(&buffer[..read])
                    .with_context(|| format!("error uploading {remote_name}"))?;
                done += read as u64;
                bytes_per_sec = meter.record(Instant::now(), done);

                on_progress(UploadProgress {
                    done,
                    total,
                    files_done: index,
                    files_total,
                    file: remote_name.to_owned(),
                    bytes_per_sec,
                });
            }

            remote
                .finish()
                .with_context(|| format!("the Xbox did not finish receiving {remote_name}"))?;

            // Return to the content folder so the next file starts from the
            // same place even when the server only accepts relative paths.
            for _ in 0..depth {
                ftp.cdup()
                    .context("cannot return to the content folder on the Xbox")?;
            }
        }

        on_progress(UploadProgress {
            done: total,
            total,
            files_done: files_total,
            files_total,
            file: String::new(),
            bytes_per_sec,
        });

        Ok(())
    })();

    let _ = ftp.quit();
    result
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
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A tiny FTP server that records commands and uploaded files.
    struct MockServer {
        port: u16,
        commands: Arc<Mutex<Vec<String>>>,
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    fn start_server() -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let files = Arc::new(Mutex::new(HashMap::new()));

        let thread_commands = Arc::clone(&commands);
        let thread_files = Arc::clone(&files);
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve(stream, thread_commands, thread_files);
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
    ) {
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
                "STOR" => {
                    writer.write_all(b"150 send it\r\n").unwrap();
                    let (mut data, _) = data_listener.take().unwrap().accept().unwrap();
                    let mut contents = Vec::new();
                    data.read_to_end(&mut contents).unwrap();
                    let key = if cwd.is_empty() {
                        argument.to_owned()
                    } else {
                        format!("{cwd}\\{argument}")
                    };
                    files.lock().unwrap().insert(key, contents);
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
        let mut meter = SpeedMeter::new(start);

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
