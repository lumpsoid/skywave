//! Head-side presentation every head shares: how a session timeline reads,
//! neutral wording for figures and outcomes, UTF-16 cursor arithmetic, and
//! the head's stateful policies. No clock and no I/O: time comes in as an
//! argument, a file to read as a reader the head opened. One Rust owner
//! per rule, so the terminal and the GUI heads cannot drift apart.
//!
//! Nothing here names a UI toolkit or a key: a head composes these with
//! its own hints, colours and layout.
//!
//! - [`blob_actions`]: when a blob may be deleted or cropped, and what a
//!   head asks first.
//! - [`clip`]: what a copy reads of a downloaded blob, within the cap.
//! - [`crop`]: the crop page's presets, selection shape and wording, and
//!   when its crop may be sent.
//! - [`sections`]: which items select, where turns start, what an item
//!   shows beneath its title, and what a copy takes.
//! - [`format`](mod@format): sizes, durations, token counts, costs, delivery,
//!   download and failure wording.
//! - [`link`]: why the link is reconnecting, and the re-pair notice for
//!   a rejection only pairing again fixes.
//! - [`output`]: where a shell run's whole output is, the download it
//!   needs, and how a head reads a downloaded one.
//! - [`usage`]: cost and tokens totalled across sessions for the usage
//!   screen.
//! - [`text`]: agent content made safe to show: control characters
//!   dropped, bidirectional controls escaped visibly.
//! - [`utf16`]: code-point cursors and byte ranges as UTF-16 code units.
//! - [`policy`]: clock-free controllers for draft saves, read marks,
//!   alerts, prompt history, workdir completion and timeline search.
//! - [`workdir`]: a session's workdir: diff line kinds, change marks and
//!   counts, entry order and labels, and every empty or failed read's
//!   wording.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod blob_actions;
pub mod clip;
pub mod crop;
pub mod format;
pub mod link;
pub mod output;
pub mod policy;
pub mod sections;
pub mod storage;
pub mod text;
pub mod usage;
pub mod utf16;
pub mod workdir;
