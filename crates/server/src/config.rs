use std::path::Path;

use serde::Deserialize;
use t7_proto::{ChannelId, ChannelInfo, DEFAULT_PORT};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub name: String,
    pub welcome: String,
    pub bind: String,
    /// Empty string = no password.
    pub password: String,
    pub max_clients: u32,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub max_clients: Option<u32>,
    /// New clients land in this channel. The first channel is used if none is marked.
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub children: Vec<ChannelConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "T7 Server".into(),
            welcome: String::new(),
            bind: format!("0.0.0.0:{DEFAULT_PORT}"),
            password: String::new(),
            max_clients: 128,
            channels: Vec::new(),
        }
    }
}

pub const DEFAULT_CONFIG: &str = r#"# T7 server configuration

name = "T7 Server"
welcome = "Welcome! Double-click a channel to join it."
bind = "0.0.0.0:9987"
# Leave empty for a public server.
password = ""
max_clients = 128

# Channels form a tree via `children`. Mark one with `default = true` to make
# it the landing channel (otherwise the first channel is used).

[[channels]]
name = "Lobby"
topic = "Hang out here"
default = true

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
name = "Music"

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
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| format!("invalid {}: {e}", path.display()))?;
        if cfg.channels.is_empty() {
            return Err("config must define at least one channel".into());
        }
        Ok(cfg)
    }

    pub fn password(&self) -> Option<&str> {
        Some(self.password.as_str()).filter(|p| !p.is_empty())
    }

    /// Flattens the channel tree. Returns the channels and the default channel id.
    pub fn flatten_channels(&self) -> (Vec<ChannelInfo>, ChannelId) {
        fn walk(
            list: &[ChannelConfig],
            parent: Option<ChannelId>,
            out: &mut Vec<ChannelInfo>,
            default: &mut Option<ChannelId>,
        ) {
            for (order, c) in list.iter().enumerate() {
                let id = out.len() as ChannelId + 1;
                out.push(ChannelInfo {
                    id,
                    parent,
                    name: c.name.clone(),
                    topic: c.topic.clone(),
                    max_clients: c.max_clients,
                    order: order as u32,
                });
                if c.default && default.is_none() {
                    *default = Some(id);
                }
                walk(&c.children, Some(id), out, default);
            }
        }
        let mut out = Vec::new();
        let mut default = None;
        walk(&self.channels, None, &mut out, &mut default);
        (out, default.unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_flattens() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(cfg.bind, "0.0.0.0:9987");
        let (channels, default) = cfg.flatten_channels();
        assert_eq!(channels.len(), 6);
        assert_eq!(default, 1);
        let squad = channels.iter().find(|c| c.name == "Squad 2").unwrap();
        assert_eq!(squad.parent, Some(2));
        assert_eq!(squad.max_clients, Some(5));
    }
}
