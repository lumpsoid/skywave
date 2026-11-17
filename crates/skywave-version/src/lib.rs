//! The build's version as every binary and head shows it: `0.1.0 (2391c1d)`,
//! the workspace version and the commit it was built from, or the bare
//! version when built outside a git checkout.
//!
//! A leaf crate of its own so that a new commit rebuilds only it and the
//! binaries that print it, not the crates under them.

/// The workspace version, from `Cargo.toml`.
pub const SEMVER: &str = env!("CARGO_PKG_VERSION");

/// The 7-character commit hash, empty outside a git checkout.
pub const COMMIT: &str = env!("SKYWAVE_COMMIT");

/// `0.1.0 (2391c1d)`, or `0.1.0` without a commit.
pub const VERSION: &str = if COMMIT.is_empty() {
    SEMVER
} else {
    concat!(env!("CARGO_PKG_VERSION"), " (", env!("SKYWAVE_COMMIT"), ")")
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_the_semver_and_a_seven_character_commit() {
        assert!(VERSION.starts_with(SEMVER));
        if COMMIT.is_empty() {
            assert_eq!(VERSION, SEMVER);
        } else {
            // git lengthens a hash only when 7 would be ambiguous.
            assert!(COMMIT.len() >= 7);
            assert!(COMMIT.chars().all(|c| c.is_ascii_hexdigit()));
            assert_eq!(VERSION, format!("{SEMVER} ({COMMIT})"));
        }
    }
}
