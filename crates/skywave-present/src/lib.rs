//! Head-side presentation every head shares: how a session timeline reads,
//! neutral wording for figures and outcomes, UTF-16 cursor arithmetic, and
//! the head's stateful policies. No clock and no I/O: time comes in as an
//! argument. One Rust owner
//! per rule, so the terminal and the GUI heads cannot drift apart.
//!
//! Nothing here names a UI toolkit or a key: a head composes these with
//! its own hints, colours and layout.
//!
//! - [`sections`]: which items select, where turns start, what an item
//!   shows beneath its title, and what a copy takes.
//! - [`format`](mod@format): sizes, durations, token counts, costs, delivery and
//!   failure wording.
//! - [`utf16`]: code-point cursors and byte ranges as UTF-16 code units.
//! - [`policy`]: clock-free controllers for draft saves, read marks,
//!   alerts, prompt history, workdir completion and timeline search.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod format;
pub mod policy;
pub mod sections;
pub mod utf16;
