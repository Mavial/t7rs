//! Over-the-air updates: the release manifest (`latest.json`) and the message
//! each build is signed with. Shared by the release tool and the client.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// `MAJOR.MINOR.PATCH`, without a leading `v`.
    pub version: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// `<os>-<arch>` as in [`current_target`], e.g. `windows-x86_64`.
    pub target: String,
    pub url: String,
    /// Hex SHA-256 of the file.
    pub sha256: String,
    /// Hex ed25519 signature of [`signed_message`].
    pub signature: String,
}

/// What gets signed: binds the file to its version and platform, so an old
/// or foreign build can't be passed off as a new one.
pub fn signed_message(version: &str, target: &str, sha256_hex: &str) -> String {
    format!("t7-update\n{version}\n{target}\n{sha256_hex}")
}

/// This build's platform, in manifest terms.
pub fn current_target() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Parses `MAJOR.MINOR.PATCH` (optionally `v`-prefixed) for comparison.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.x.3"), None);
    }

    #[test]
    fn hex_roundtrips_and_rejects_junk() {
        assert_eq!(from_hex(&to_hex(&[0, 1, 0xab, 0xff])), Some(vec![0, 1, 0xab, 0xff]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
