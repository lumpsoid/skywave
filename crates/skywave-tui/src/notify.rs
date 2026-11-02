//! Alerting the user through the terminal when an approval comes in, or a
//! long shell run of this device's finishes: a bell
//! (BEL), which every terminal and tmux pass on, and/or a desktop
//! notification with OSC 9 (iTerm2, kitty, WezTerm, foot, Ghostty) or
//! OSC 777 (rxvt, VTE terminals). Inside tmux the OSC ones are wrapped for
//! DCS passthrough, which needs `set -g allow-passthrough on`.
//!
//! [`App`](crate::App) decides when to alert
//! ([`take_alert`](crate::App::take_alert)); [`send`] does the I/O for
//! [`run`](crate::run).
//! An approval's alert names the session, never what the agent wants to
//! run; a shell run's names the command this device asked for.

use std::io::{self, Write};

use crossterm::execute;
use crossterm::style::Print;
/// The longest session title sent, and how a title is made safe to send:
/// shared with the other heads' alerts.
pub use skywave_present::policy::{TITLE_MAX, sanitise};

/// Environment variable used when `--notify` is not given.
pub const VAR: &str = "SKYWAVE_NOTIFY";

/// How to alert; any combination, all off for `off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notify {
    pub bell: bool,
    pub osc9: bool,
    pub osc777: bool,
}

impl Default for Notify {
    /// The bell only: it works everywhere, tmux included.
    fn default() -> Self {
        Notify {
            bell: true,
            osc9: false,
            osc777: false,
        }
    }
}

impl Notify {
    pub const OFF: Notify = Notify {
        bell: false,
        osc9: false,
        osc777: false,
    };

    /// Parses `off` or a comma-separated list of `bell`, `osc9`, `osc777`.
    pub fn parse(text: &str) -> Result<Notify, String> {
        let text = text.trim();
        if text == "off" {
            return Ok(Notify::OFF);
        }
        let mut notify = Notify::OFF;
        for part in text.split(',').map(str::trim) {
            match part {
                "bell" => notify.bell = true,
                "osc9" => notify.osc9 = true,
                "osc777" => notify.osc777 = true,
                other => {
                    return Err(format!(
                        "unknown alert {other:?}: use off, or bell, osc9, osc777 separated by commas"
                    ));
                }
            }
        }
        Ok(notify)
    }
}

/// The notification's text for an approval in the session titled `title`.
pub fn message(title: Option<&str>) -> String {
    match title.map(sanitise).filter(|t| !t.is_empty()) {
        Some(title) => format!("skywave: approval needed · {title}"),
        None => "skywave: approval needed".to_owned(),
    }
}

/// The notification's text for a long shell run of this device's that
/// finished. `notice` is [`ShellNotice::notice`](skywave_present::policy::ShellNotice::notice):
/// its parts are [`sanitise`]d, so it is safe to send.
pub fn shell_message(notice: &str) -> String {
    format!("skywave: {notice}")
}

/// Wraps `sequence` for tmux's DCS passthrough: its escapes doubled.
pub fn tmux_wrap(sequence: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
}

/// Everything `notify` writes for `message`, which must be [`sanitise`]d.
pub fn sequences(notify: Notify, message: &str, tmux: bool) -> String {
    let osc = |sequence: String| if tmux { tmux_wrap(&sequence) } else { sequence };
    let mut out = String::new();
    if notify.osc9 {
        out.push_str(&osc(format!("\x1b]9;{message}\x07")));
    }
    if notify.osc777 {
        out.push_str(&osc(format!("\x1b]777;notify;skywave;{message}\x07")));
    }
    if notify.bell {
        out.push('\x07');
    }
    out
}

/// Whether we run inside tmux.
pub fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

/// Alerts through `out` with `message`, made by [`message`] or
/// [`shell_message`].
pub fn send(out: &mut impl Write, notify: Notify, message: &str, tmux: bool) -> io::Result<()> {
    let sequences = sequences(notify, message, tmux);
    if sequences.is_empty() {
        return Ok(());
    }
    execute!(out, Print(sequences))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_any_combination_or_off() {
        assert_eq!(Notify::parse("off"), Ok(Notify::OFF));
        assert_eq!(Notify::parse("bell"), Ok(Notify::default()));
        assert_eq!(
            Notify::parse("osc9, bell"),
            Ok(Notify {
                bell: true,
                osc9: true,
                osc777: false
            })
        );
        assert_eq!(
            Notify::parse("osc777"),
            Ok(Notify {
                bell: false,
                osc9: false,
                osc777: true
            })
        );
        assert!(Notify::parse("loud").is_err());
        assert!(Notify::parse("bell,off").is_err());
    }

    #[test]
    fn titles_lose_controls_and_semicolons_and_are_capped() {
        assert_eq!(sanitise("a\x1b]0;x\x07b\u{9b}c;d\n"), "a]0xbcd");
        assert_eq!(sanitise(&"é".repeat(150)).chars().count(), TITLE_MAX);
        assert_eq!(message(Some("app")), "skywave: approval needed · app");
        assert_eq!(message(Some("\x07")), "skywave: approval needed");
        assert_eq!(message(None), "skywave: approval needed");
    }

    #[test]
    fn sequences_per_method_and_inside_tmux() {
        let all = Notify {
            bell: true,
            osc9: true,
            osc777: true,
        };
        assert_eq!(
            sequences(all, "hi", false),
            "\x1b]9;hi\x07\x1b]777;notify;skywave;hi\x07\x07"
        );
        assert_eq!(
            sequences(all, "hi", true),
            "\x1bPtmux;\x1b\x1b]9;hi\x07\x1b\\\x1bPtmux;\x1b\x1b]777;notify;skywave;hi\x07\x1b\\\x07"
        );
        assert_eq!(sequences(Notify::OFF, "hi", false), "");
        let mut out = Vec::new();
        send(&mut out, Notify::OFF, &message(Some("app")), false).unwrap();
        assert!(out.is_empty());
    }
}
