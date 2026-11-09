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
//! - [`limits`]: the usage limits page: a row per limit, when it was
//!   read, and why there is nothing to list.
//! - [`link`]: why the link is reconnecting, and the re-pair notice for
//!   a rejection only pairing again fixes.
//! - [`login`]: the warning that the agent's login expires soon or is
//!   gone, and where a login from this device stands.
//! - [`markdown`]: an agent message as flat blocks of styled spans, and
//!   which of its links a head may open.
//! - [`output`]: where a shell run's whole output is, the download it
//!   needs, and how a head reads a downloaded one.
//! - [`session_info`]: a session's info page: what it is, what its turns
//!   took and by which models, how its agent is set, and its subagents.
//! - [`usage`]: cost and tokens totalled across sessions for the usage
//!   screen.
//! - [`text`]: agent content made safe to show: control characters
//!   dropped, bidirectional controls escaped visibly.
//! - [`utf16`]: code-point cursors and byte ranges as UTF-16 code units.
//! - [`policy`]: clock-free controllers for draft saves, read marks,
//!   alerts, prompt history, workdir completion and timeline search.
//! - [`question`]: answering a question the agent asks: the draft a head
//!   keeps while the user picks, and how an answer reads.
//! - [`workdir`]: a session's workdir: diff line kinds, change marks and
//!   counts, entry order and labels, and every empty or failed read's
//!   wording.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod blob_actions;
pub mod clip;
pub mod crop;
pub mod format;
pub mod limits;
pub mod link;
pub mod login;
pub mod markdown;
pub mod output;
pub mod policy;
pub mod question;
pub mod sections;
pub mod session_info;
pub mod storage;
pub mod text;
pub mod usage;
pub mod utf16;
pub mod workdir;
