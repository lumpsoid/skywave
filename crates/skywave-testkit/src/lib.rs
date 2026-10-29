//! Test-only helpers for Skywave. Never a normal dependency of a product
//! crate; list it under `[dev-dependencies]`.
//!
//! - [`LinkProxy`]: the link harness (spec §9, D11) — an in-process TCP proxy
//!   that injects latency, jitter, a bandwidth cap and connection drops
//!   between a client and a server, adjustable while it runs.
//! - [`Rng`] and [`seed`]: a small seedable generator, so a failing run can
//!   be repeated with `SKYWAVE_LINK_SEED=<seed>`.

mod proxy;
mod rng;

pub use proxy::{LinkProfile, LinkProxy, LinkStats};
pub use rng::{Rng, SEED_ENV, seed};
