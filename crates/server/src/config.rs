use std::path::Path;

use serde::{Deserialize, Serialize};
use t7_proto::perms::Override;
use t7_proto::{ChannelKind, DEFAULT_PORT};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub name: String,
    pub welcome: String,
    pub bind: String,
    /// Empty string = no password.
    pub password: String,
    pub max_clients: u32,
    pub admin_token: Option<serde::de::IgnoredAny>,
    /// Seeds `channels.toml` on first start; after that, that file is used.
    pub channels: Vec<ChannelConfig>,
}

/// One channel, as written in the config and in `channels.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_clients: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// New clients land in this channel. The first channel is used if none is marked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// Identities (hex) that hold the Moderator role in this channel and the ones below it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moderators: Vec<t7_proto::Identity>,
    /// `"category"`: a heading at the top level for the channels under it, which nobody joins.
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: ChannelKind,
    /// Per role (0 is Everyone), the permissions this channel allows and
    /// denies over the channel above it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<Override>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChannelConfig>,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "T7 Server".into(),
            welcome: String::new(),
            bind: format!("0.0.0.0:{DEFAULT_PORT}"),
            password: String::new(),
            max_clients: 128,
            admin_token: None,
            channels: Vec::new(),
        }
    }
}

pub const DEFAULT_CONFIG: &str = r#"# T7 server configuration

name = "T7 Server"
welcome = "Welcome! Double-click a channel to join it. Right-click for more."
bind = "0.0.0.0:9988"
# Leave empty for a public server.
password = ""
max_clients = 128
# On first start the log shows a claim token. In T7, the menu next to
# CHANNELS, then Claim ownership, makes whoever enters it first the owner.
# Roles and who holds them are kept in roles.toml next to this file.

# The channels a new server starts with. On first start they are copied to
# channels.toml next to this file, which then holds the live channel tree as
# it is edited in T7. Delete channels.toml to start over from these.
# Channels nest via `children`; `default = true` marks where people land.
# `kind = "category"` makes a top-level heading for the channels in it,
# which nobody joins. Who may see or do what in a channel is easiest to
# set in T7: right-click the channel, then Edit channel, then Permissions.

[[channels]]
name = "Lobby"
topic = "Say hi"
default = true

[[channels]]
name = "Hangout"

[[channels]]
name = "Gaming"
topic = "Squad up"

  [[channels.children]]
  name = "Squad 1"
  max_clients = 5

  [[channels.children]]
  name = "Squad 2"
  max_clients = 5

[[channels]]
name = "AFK"
topic = "Away from keyboard"
"#;

impl Config {
    /// Loads the config, writing the default file first if it doesn't exist.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            std::fs::write(path, DEFAULT_CONFIG)
                .map_err(|e| format!("cannot write default config {}: {e}", path.display()))?;
            tracing::info!("wrote default config to {}", path.display());
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("invalid {}: {e}", path.display()))
    }

    pub fn password(&self) -> Option<&str> {
        Some(self.password.as_str()).filter(|p| !p.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::Channels;

    #[test]
    fn default_config_builds_a_channel_tree() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(cfg.bind, "0.0.0.0:9988");
        let channels = Channels::from_config(&cfg.channels).unwrap();
        let all: Vec<_> = t7_proto::perms::ChannelTree::channels(&channels).collect();
        assert_eq!(all.len(), 6);
        assert_eq!(channels.get(channels.default_id()).unwrap().name, "Lobby");
        let squad = all.iter().find(|c| c.name == "Squad 2").unwrap();
        assert_eq!(channels.get(squad.parent.unwrap()).unwrap().name, "Gaming");
        assert_eq!(squad.max_clients, Some(5));
        // Saving and reloading keeps the tree.
        assert_eq!(Channels::from_config(&channels.to_config()).unwrap().to_config(), channels.to_config());
    }
}
