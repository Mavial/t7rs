//! Client settings, stored as TOML in the user's config directory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use t7_proto::ClientStatus;
use t7_proto::identity::IdentityKey;
use t7_proto::tls::Fingerprint;

use crate::audio::AudioSettings;

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    pub nickname: String,
    pub status: ClientStatus,
    pub audio: AudioSettings,
    pub recent: Vec<Recent>,
    /// Per-person volume by nickname; only values other than 100% are kept.
    pub user_volumes: std::collections::BTreeMap<String, f32>,
    /// Certificate fingerprint per `net::canonical_address`, pinned on first connect.
    pub server_certs: std::collections::BTreeMap<String, Fingerprint>,
    pub folded: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Who this user is on every server: roles, bans and moderator rights follow it.
    /// Made on the first connect.
    pub identity_key: Option<IdentityKey>,
}

/// A new identity, from the system's randomness.
pub fn new_identity_key() -> IdentityKey {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("no system randomness");
    IdentityKey::from_seed(seed)
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Recent {
    pub address: String,
    pub server_name: String,
}

/// Remembers what is on disk so `save` only writes when something changed.
pub struct Store {
    path: Option<PathBuf>,
    saved: Settings,
}

impl Store {
    /// `profile` selects a separate settings file, e.g. to run two clients on one machine.
    pub fn open(profile: Option<&str>) -> (Self, Settings) {
        let file = match profile {
            Some(p) => format!("settings-{p}.toml"),
            None => "settings.toml".to_owned(),
        };
        let path = data_dir().map(|d| d.join(file));
        let settings: Settings = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| match toml::from_str(&text) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!("ignoring unreadable settings file: {e}");
                    None
                }
            })
            .unwrap_or_default();
        (Self { path, saved: settings.clone() }, settings)
    }

    pub fn save(&mut self, settings: &Settings) {
        if *settings == self.saved {
            return;
        }
        let Some(path) = &self.path else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let text = toml::to_string(settings).map_err(std::io::Error::other)?;
            // Write-then-rename, so a crash mid-write never leaves a truncated file.
            let tmp = path.with_extension("toml.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, path)
        };
        match write() {
            Ok(()) => self.saved = settings.clone(),
            Err(e) => tracing::warn!("could not save settings to {}: {e}", path.display()),
        }
    }
}

/// T7's folder in the user's config directory.
pub fn data_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("T7"))
}

fn config_dir() -> Option<PathBuf> {
    let env = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip() {
        let s = Settings {
            nickname: "santi".into(),
            status: ClientStatus { muted: true, deafened: false, away: false },
            audio: AudioSettings { ptt_key: Some(crate::audio::PttKey { vk: 0x56 }), ..Default::default() },
            recent: vec![Recent { address: "1.2.3.4".into(), server_name: "T7 Server".into() }],
            user_volumes: [("Some One".to_owned(), 0.5)].into(),
            server_certs: [("1.2.3.4:9988".to_owned(), Fingerprint([0xab; 32]))].into(),
            folded: [("1.2.3.4:9988".to_owned(), ["Games".to_owned()].into())].into(),
            identity_key: Some(new_identity_key()),
        };
        let text = toml::to_string(&s).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), s);
    }
}
