//! The Android backend (D65): drives devices through the adb executable. It
//! is the one place the screen runner names adb (D3).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use skywave_proto::{ScreenDevice, ScreenKey};
use tokio::process::Command;

use super::backend::{BackendError, ScreenBackend};

/// How long an adb command may run before it is killed.
const TIMEOUT: Duration = Duration::from_secs(30);
/// How long an install may run before it is killed.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
/// The first eight bytes of every PNG file.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The Android backend: drives devices through the adb executable at `adb`.
pub struct Android {
    adb: PathBuf,
}

impl Android {
    /// A backend that runs `adb`: a path, or a bare name looked up on `PATH`.
    pub fn new(adb: impl Into<PathBuf>) -> Android {
        Android { adb: adb.into() }
    }

    /// Runs adb with `args` and returns its stdout. A failed run gives its
    /// trimmed stderr, or its exit status when stderr is empty. The child is
    /// killed when the timeout expires.
    async fn adb(&self, args: &[&str], timeout: Duration) -> Result<Vec<u8>, BackendError> {
        let child = Command::new(&self.adb)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| BackendError(format!("cannot run {}: {e}", self.adb.display())))?;
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| BackendError(format!("adb timed out after {} s", timeout.as_secs())))?
            .map_err(|e| BackendError(format!("cannot run {}: {e}", self.adb.display())))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            return Err(BackendError(if stderr.is_empty() {
                format!("adb exited with {}", output.status)
            } else {
                stderr.to_owned()
            }));
        }
        Ok(output.stdout)
    }
}

impl ScreenBackend for Android {
    async fn devices(&mut self) -> Result<Vec<ScreenDevice>, BackendError> {
        let out = self.adb(&["devices", "-l"], TIMEOUT).await?;
        Ok(parse_devices(&String::from_utf8_lossy(&out)))
    }

    async fn capture(&mut self, device: &str) -> Result<Vec<u8>, BackendError> {
        let png = self
            .adb(&["-s", device, "exec-out", "screencap", "-p"], TIMEOUT)
            .await?;
        if png.starts_with(PNG_SIGNATURE) {
            Ok(png)
        } else {
            Err(BackendError("the screen capture is not a PNG".to_owned()))
        }
    }

    async fn tap(&mut self, device: &str, x: u32, y: u32) -> Result<(), BackendError> {
        let (x, y) = (x.to_string(), y.to_string());
        self.adb(&["-s", device, "shell", "input", "tap", &x, &y], TIMEOUT)
            .await?;
        Ok(())
    }

    async fn swipe(
        &mut self,
        device: &str,
        from: (u32, u32),
        to: (u32, u32),
        duration_ms: u32,
    ) -> Result<(), BackendError> {
        let [x1, y1, x2, y2] = [from.0, from.1, to.0, to.1].map(|n| n.to_string());
        let duration = duration_ms.to_string();
        let args = [
            "-s", device, "shell", "input", "swipe", &x1, &y1, &x2, &y2, &duration,
        ];
        self.adb(&args, TIMEOUT).await?;
        Ok(())
    }

    async fn text(&mut self, device: &str, text: &str) -> Result<(), BackendError> {
        let escaped = escape_text(text);
        self.adb(&["-s", device, "shell", "input", "text", &escaped], TIMEOUT)
            .await?;
        Ok(())
    }

    async fn key(&mut self, device: &str, key: ScreenKey) -> Result<(), BackendError> {
        let code = keycode(key).to_string();
        self.adb(
            &["-s", device, "shell", "input", "keyevent", &code],
            TIMEOUT,
        )
        .await?;
        Ok(())
    }

    async fn install(&mut self, device: &str, apk: &Path) -> Result<(), BackendError> {
        let apk = apk
            .to_str()
            .ok_or_else(|| BackendError("the APK path is not UTF-8".to_owned()))?;
        self.adb(&["-s", device, "install", "-r", apk], INSTALL_TIMEOUT)
            .await?;
        Ok(())
    }

    async fn launch(&mut self, device: &str, app: &str) -> Result<(), BackendError> {
        // `am` and `monkey` can exit 0 on failure, so the output is checked.
        let out = if app.contains('/') {
            self.adb(&["-s", device, "shell", "am", "start", "-n", app], TIMEOUT)
                .await?
        } else {
            let args = [
                "-s",
                device,
                "shell",
                "monkey",
                "-p",
                app,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ];
            self.adb(&args, TIMEOUT).await?
        };
        let stdout = String::from_utf8_lossy(&out);
        let failed = stdout
            .lines()
            .find(|line| line.contains("Error") || line.contains("No activities found"));
        match failed {
            Some(line) => Err(BackendError(line.trim().to_owned())),
            None => Ok(()),
        }
    }

    async fn stop(&mut self, device: &str, app: &str) -> Result<(), BackendError> {
        self.adb(&["-s", device, "shell", "am", "force-stop", app], TIMEOUT)
            .await?;
        Ok(())
    }
}

/// The devices in `adb devices -l` output. Lines starting with `*` are daemon
/// notices, which adb may print before the `List of devices attached` header,
/// so the header is skipped by its text, not by its position. A line without
/// a serial and a state is skipped, which also drops blank lines.
fn parse_devices(out: &str) -> Vec<ScreenDevice> {
    out.lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !line.starts_with('*'))
        .filter(|line| !line.starts_with("List of devices"))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?;
            let state = fields.next()?;
            let model = fields.find_map(|field| field.strip_prefix("model:"));
            Some(ScreenDevice {
                serial: serial.to_owned(),
                state: state.to_owned(),
                model: model.map(str::to_owned),
            })
        })
        .collect()
}

/// Escapes `text` for `input text`. The device shell splits the arguments
/// again, so a space becomes `%s` and every other character that is not
/// alphanumeric is backslash-escaped, which keeps the text one literal word.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            ' ' => out.push_str("%s"),
            c if c.is_ascii_alphanumeric() => out.push(c),
            c => {
                out.push('\\');
                out.push(c);
            }
        }
    }
    out
}

/// The Android keycode of `key`.
fn keycode(key: ScreenKey) -> u32 {
    match key {
        ScreenKey::Back => 4,
        ScreenKey::Home => 3,
        ScreenKey::Recents => 187,
        ScreenKey::Enter => 66,
        ScreenKey::Delete => 67,
        ScreenKey::Power => 26,
        ScreenKey::VolumeUp => 24,
        ScreenKey::VolumeDown => 25,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_device_list() {
        let sample = "List of devices attached\n\
            emulator-5554          device product:sdk model:Pixel_7 device:emu64a transport_id:1\n\
            R58M  unauthorized usb:1-1 transport_id:2\n\n";
        assert_eq!(
            parse_devices(sample),
            vec![
                ScreenDevice {
                    serial: "emulator-5554".to_owned(),
                    state: "device".to_owned(),
                    model: Some("Pixel_7".to_owned()),
                },
                ScreenDevice {
                    serial: "R58M".to_owned(),
                    state: "unauthorized".to_owned(),
                    model: None,
                },
            ]
        );
    }

    #[test]
    fn skips_daemon_notices() {
        let sample = "* daemon not running; starting now at tcp:5037\n\
            List of devices attached\nR58M  device\n";
        assert_eq!(parse_devices(sample).len(), 1);
    }

    #[test]
    fn escapes_a_space_as_percent_s() {
        assert_eq!(escape_text("a b"), "a%sb");
    }

    #[test]
    fn escapes_a_quote() {
        assert_eq!(escape_text("it's"), "it\\'s");
    }
}
