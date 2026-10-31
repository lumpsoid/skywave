use rinf::RustSignal;
use serde::Serialize;

/// Something for Dart to do once: a command, not state. Dart performs it
/// and keeps nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, RustSignal)]
pub enum PlatformEffect {
    /// New approvals came in: a system notification. `title` is the
    /// newest one's session, sanitised (may be empty); `body` the line
    /// to show, `approval needed · <title>`; `session_id` the session to
    /// open on a tap.
    Notify {
        title: String,
        body: String,
        session_id: Option<String>,
    },
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
}
