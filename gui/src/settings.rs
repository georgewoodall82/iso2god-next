//! Persisted application settings.
//!
//! Settings live in a small JSON file under the user's config directory so the
//! FTP details survive restarts. The password is stored in plain text, matching
//! what other Xbox 360 transfer tools do; the file is only readable by the user.

use std::fs;
use std::io;
use std::path::PathBuf;

use relm4::gtk::glib;
use serde::{Deserialize, Serialize};

/// Remote folder on the console that receives converted games. `Hdd1:` is the
/// internal hard drive and `0000000000000000` is the content type Microsoft
/// uses for installed games.
pub const DEFAULT_REMOTE_DIR: &str = r"Hdd1:\Content\0000000000000000";

/// Where each converted package is uploaded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FtpSettings {
    /// Upload converted games after each conversion.
    pub enabled: bool,
    /// Address of the console, optionally with a port (`192.168.1.23:21`).
    pub host: String,
    pub username: String,
    pub password: String,
    /// Folder on the console that receives the converted games.
    pub remote_dir: String,
}

impl Default for FtpSettings {
    fn default() -> Self {
        FtpSettings {
            enabled: false,
            host: String::new(),
            username: "xboxftp".to_owned(),
            password: "xboxftp".to_owned(),
            remote_dir: DEFAULT_REMOTE_DIR.to_owned(),
        }
    }
}

impl FtpSettings {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return FtpSettings::default();
        };

        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
                eprintln!(
                    "iso2god-next-gui: ignoring unreadable settings in {}: {error}",
                    path.display()
                );
                FtpSettings::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => FtpSettings::default(),
            Err(error) => {
                eprintln!(
                    "iso2god-next-gui: cannot read settings from {}: {error}",
                    path.display()
                );
                FtpSettings::default()
            }
        }
    }

    /// Writes the settings atomically so a crash never leaves a truncated file.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = config_path() else {
            return Err(io::Error::other("no config directory available"));
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        write_private(&temporary, json.as_bytes())?;
        fs::rename(&temporary, &path)
    }

    /// Address and port of the FTP server, defaulting to port 21.
    pub fn server(&self) -> (String, u16) {
        let host = self.host.trim();
        if let Some((host, port)) = host.rsplit_once(':')
            && !host.is_empty()
            && !host.contains(':')
            && let Ok(port) = port.parse::<u16>()
        {
            return (host.to_owned(), port);
        }
        (host.to_owned(), 21)
    }

    /// Whether an upload can be attempted (an address is present).
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty()
    }

    /// Remote folder for converted games, falling back to the stock location.
    pub fn remote_dir(&self) -> &str {
        let dir = self.remote_dir.trim();
        if dir.is_empty() {
            DEFAULT_REMOTE_DIR
        } else {
            dir
        }
    }
}

fn config_path() -> Option<PathBuf> {
    Some(
        glib::user_config_dir()
            .join("iso2god-next")
            .join("ftp.json"),
    )
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    // `mode` only applies when the file is created; tighten an existing one too.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, contents: &[u8]) -> io::Result<()> {
    fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_xbox_defaults() {
        let settings: FtpSettings =
            serde_json::from_str(r#"{"enabled":true,"host":"192.168.1.23"}"#).unwrap();

        assert!(settings.enabled);
        assert_eq!(settings.username, "xboxftp");
        assert_eq!(settings.password, "xboxftp");
        assert_eq!(settings.remote_dir(), DEFAULT_REMOTE_DIR);
        assert_eq!(settings.server(), ("192.168.1.23".to_owned(), 21));
    }

    #[test]
    fn server_accepts_an_optional_port() {
        let settings = FtpSettings {
            host: "192.168.1.23:2121".to_owned(),
            ..FtpSettings::default()
        };
        assert_eq!(settings.server(), ("192.168.1.23".to_owned(), 2121));
    }
}
