use rinf::RustSignal;
use serde::Serialize;

/// Something for Dart to do once: a command, not state. Dart performs it
/// and keeps nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, RustSignal)]
pub enum PlatformEffect {
    /// New approvals came in, or a long shell run finished: a system
    /// notification. `id` is the notification's, one per session and kind
    /// (showing it again replaces it); `title` is the session's, sanitised
    /// (may be empty); `body` the line to show, `approval needed · <title>`;
    /// `session_id` the session to open on a tap.
    Notify {
        id: i32,
        title: String,
        body: String,
        session_id: Option<String>,
    },
    /// Take down notification `id` if it is up: its session no longer
    /// waits on an approval (answered or cancelled from any device, or the
    /// session is gone).
    Dismiss { id: i32 },
    /// Put `text` on the clipboard.
    CopyToClipboard { text: String },
    /// Open the file at `path` with the system's viewer.
    OpenFile { path: String },
    /// A passing note, e.g. `copied 4.0 KiB`, `downloading 1.2 MiB · copy
    /// again when it is here`.
    Notice { text: String },
    /// Something failed: a dispatch the core refused, a malformed pairing
    /// URI, an unreadable file.
    Error { text: String },
    /// Start (`run`) or stop the Android stay-connected service. Asked,
    /// not assumed: Kotlin reports the outcome back as
    /// `GuiIntent::BackgroundService`, and only that report holds the link.
    BackgroundService { run: bool },
}
