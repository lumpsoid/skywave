//! The GUI head's own preferences, kept by the hub in a file beside the
//! core's data dir: `<app support>/skywave-gui.json`, not inside
//! `<app support>/skywave`, so a replica reset leaves them alone. They are
//! this device's head's, not the core's: no other head and no server sees
//! them.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The preferences file under the directory Dart sends.
pub const PREFS_FILE: &str = "skywave-gui.json";

/// What the user chose. A missing field, file or a file that cannot be read
/// means the defaults: Stay connected off, every notification on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Keep the link open with the app in the background, through the
    /// Android foreground service (D21).
    pub stay_connected: bool,
    /// Show the session list as one plain list, most recently active first,
    /// instead of grouped by project.
    pub plain_list: bool,
    /// Which alerts become system notifications (Settings → Notifications).
    pub notifications: NotificationPrefs,
}

/// The notification switches. A notice of a kind is shown only when `all`
/// and that kind are on. Switching off stops new notices alone: a notice
/// already shown stays, and dismissals still go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationPrefs {
    /// Every notification; off stops all of them below.
    pub all: bool,
    /// A session needs an approval.
    pub approvals: bool,
    /// A session's agent went from working to idle.
    pub idle: bool,
    /// A long shell run of this device's finished.
    pub shell: bool,
}

impl NotificationPrefs {
    /// Every switch on: what a fresh install and an older file get.
    pub const ON: NotificationPrefs = NotificationPrefs {
        all: true,
        approvals: true,
        idle: true,
        shell: true,
    };
}

impl Default for NotificationPrefs {
    fn default() -> Self {
        Self::ON
    }
}

/// The preferences file for the directory Dart sent.
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(PREFS_FILE)
}

/// Reads the preferences at `path`. A missing file is the defaults; so is
/// one that cannot be read or parsed, logged: a lost preference is not
/// worth failing the boot over.
pub fn load(path: &Path) -> Prefs {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Prefs::default(),
        Err(err) => {
            tracing::warn!(path = %path.display(), "cannot read the preferences, using the defaults: {err}");
            return Prefs::default();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|err| {
        tracing::warn!(path = %path.display(), "unreadable preferences, using the defaults: {err}");
        Prefs::default()
    })
}

/// Writes `prefs` to `path` whole or not at all: to a temporary file in the
/// same directory, synced, then renamed over it.
pub fn save(path: &Path, prefs: &Prefs) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(prefs).map_err(io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&path_in(dir.path())), Prefs::default());
        assert!(!Prefs::default().stay_connected);
        assert_eq!(Prefs::default().notifications, NotificationPrefs::ON);
    }

    #[test]
    fn a_corrupt_file_is_the_defaults_and_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        for corrupt in ["not json", "{\"stay_connected\": \"yes\"}", ""] {
            fs::write(&path, corrupt).unwrap();
            assert_eq!(load(&path), Prefs::default(), "{corrupt:?}");
        }
        // A directory in its place cannot be read either.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(load(&path), Prefs::default());
    }

    #[test]
    fn an_older_file_keeps_notifications_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        fs::write(&path, r#"{"stay_connected":true}"#).unwrap();
        let loaded = load(&path);
        assert!(loaded.stay_connected);
        assert_eq!(loaded.notifications, NotificationPrefs::ON);
    }

    #[test]
    fn it_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let on = Prefs {
            stay_connected: true,
            plain_list: true,
            notifications: NotificationPrefs {
                idle: false,
                ..NotificationPrefs::ON
            },
        };
        save(&path, &on).unwrap();
        assert_eq!(load(&path), on);
        save(&path, &Prefs::default()).unwrap();
        assert_eq!(load(&path), Prefs::default());
        // Only the file itself is left: the temporary one was renamed.
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![PREFS_FILE]);
    }

    #[test]
    fn the_file_says_what_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        fs::write(&path, r#"{"stay_connected": true}"#).unwrap();
        assert!(load(&path).stay_connected);
        // A field this build does not know is ignored.
        fs::write(&path, r#"{"stay_connected": true, "later": 1}"#).unwrap();
        assert!(load(&path).stay_connected);
    }

    #[test]
    fn plain_list_is_kept_and_defaults_to_grouped() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        // A file from before the field existed loads grouped.
        fs::write(&path, r#"{"stay_connected": true}"#).unwrap();
        assert!(!load(&path).plain_list);
        fs::write(&path, r#"{"plain_list": true}"#).unwrap();
        assert_eq!(
            load(&path),
            Prefs {
                stay_connected: false,
                plain_list: true,
                notifications: NotificationPrefs::ON,
            }
        );
    }

    #[test]
    fn a_failed_save_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        // The rename cannot replace a non-empty directory.
        fs::create_dir(&path).unwrap();
        fs::write(path.join("x"), b"").unwrap();
        assert!(save(&path, &Prefs::default()).is_err());
        assert!(!dir.path().join("skywave-gui.json.tmp").exists());
    }
}
