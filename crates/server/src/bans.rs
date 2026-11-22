//! The ban list and its saved format. Pure state, no networking, and no
//! clock: callers pass the time, in Unix seconds.

use std::net::IpAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};
use t7_proto::{BanId, BanInfo, Identity};

/// `bans.toml`.
#[derive(Default, Serialize, Deserialize)]
struct BansFile {
    #[serde(default)]
    bans: Vec<BanInfo>,
}

/// Who is banned, as the admin asked for it.
pub struct NewBan {
    pub identity: Identity,
    pub ip: Option<IpAddr>,
    pub nickname: String,
    pub reason: String,
    pub banned_by: String,
    /// `None`: for good.
    pub duration_secs: Option<u64>,
}

#[derive(Default)]
pub struct Bans {
    list: Vec<BanInfo>,
}

impl Bans {
    pub fn load(path: &Path, now: u64) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let file: BansFile = toml::from_str(&text).map_err(std::io::Error::other)?;
        let mut bans = Self { list: file.bans };
        bans.prune(now);
        Ok(bans)
    }

    /// Write-then-rename, so a crash never truncates the file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let header = "# Banned identities and IP addresses, kept up to date by the server as\n\
                      # admins ban and unban people in T7. Edit it only while the server is stopped.\n\n";
        let body = toml::to_string(&BansFile { bans: self.list.clone() }).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, format!("{header}{body}"))?;
        std::fs::rename(&tmp, path)
    }

    pub fn add(&mut self, ban: NewBan, now: u64) -> &BanInfo {
        let id = self.list.iter().map(|b| b.id).max().unwrap_or(0) + 1;
        self.list.push(BanInfo {
            id,
            identity: ban.identity,
            ip: ban.ip.map(|ip| ip.to_canonical()),
            nickname: ban.nickname,
            reason: ban.reason,
            banned_by: ban.banned_by,
            created_unix: now,
            expires_unix: ban.duration_secs.map(|d| now.saturating_add(d)),
        });
        self.list.last().expect("just pushed")
    }

    pub fn remove(&mut self, id: BanId) -> Option<BanInfo> {
        let at = self.list.iter().position(|b| b.id == id)?;
        Some(self.list.remove(at))
    }

    /// The ban that keeps this identity, or anyone at this address, out right now.
    pub fn find(&self, identity: Identity, ip: IpAddr, now: u64) -> Option<&BanInfo> {
        let ip = ip.to_canonical();
        self.list.iter().find(|b| active(b, now) && (b.identity == identity || b.ip == Some(ip)))
    }

    /// The bans still in force. Expired ones are dropped; returns whether any were.
    pub fn prune(&mut self, now: u64) -> bool {
        let before = self.list.len();
        self.list.retain(|b| active(b, now));
        self.list.len() != before
    }

    pub fn list(&self) -> &[BanInfo] {
        &self.list
    }
}

fn active(ban: &BanInfo, now: u64) -> bool {
    ban.expires_unix.is_none_or(|end| now < end)
}

/// Rounded down to the largest whole unit: "3 days", "1 hour", "less than a minute".
pub fn describe_duration(secs: u64) -> String {
    let (n, unit) = match secs {
        s if s >= 86_400 => (s / 86_400, "day"),
        s if s >= 3_600 => (s / 3_600, "hour"),
        s if s >= 60 => (s / 60, "minute"),
        _ => return "less than a minute".into(),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: Identity = Identity([1; 32]);
    const BOB: Identity = Identity([2; 32]);

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn ban(identity: Identity, ip: Option<IpAddr>, duration_secs: Option<u64>) -> NewBan {
        NewBan { identity, ip, nickname: "x".into(), reason: "spam".into(), banned_by: "admin".into(), duration_secs }
    }

    #[test]
    fn identity_bans_follow_the_key_to_any_address() {
        let mut bans = Bans::default();
        bans.add(ban(ALICE, None, None), 100);
        assert_eq!(bans.find(ALICE, ip("10.0.0.1"), 100).map(|b| b.id), Some(1));
        assert!(bans.find(ALICE, ip("192.168.1.9"), u64::MAX).is_some(), "permanent");
        assert!(bans.find(BOB, ip("10.0.0.1"), 100).is_none(), "another key at the same address");
    }

    #[test]
    fn ip_bans_keep_out_new_keys_from_that_address() {
        let mut bans = Bans::default();
        bans.add(ban(ALICE, Some(ip("10.0.0.1")), None), 100);
        assert!(bans.find(BOB, ip("10.0.0.1"), 100).is_some());
        assert!(bans.find(BOB, ip("::ffff:10.0.0.1"), 100).is_some(), "the same IPv4 address seen over IPv6");
        assert!(bans.find(BOB, ip("10.0.0.2"), 100).is_none());
    }

    #[test]
    fn temporary_bans_end() {
        let mut bans = Bans::default();
        bans.add(ban(ALICE, Some(ip("10.0.0.1")), Some(60)), 100);
        assert!(bans.find(ALICE, ip("10.0.0.1"), 159).is_some());
        assert!(bans.find(ALICE, ip("10.0.0.1"), 160).is_none());
        assert!(bans.find(BOB, ip("10.0.0.1"), 160).is_none(), "the IP ban ends too");
        assert_eq!(bans.list().len(), 1, "finding doesn't prune");
        assert!(bans.prune(160));
        assert!(bans.list().is_empty());
        assert!(!bans.prune(160));
    }

    #[test]
    fn unban_and_ids() {
        let mut bans = Bans::default();
        let first = bans.add(ban(ALICE, None, None), 100).id;
        let second = bans.add(ban(BOB, None, None), 100).id;
        assert_ne!(first, second);
        assert_eq!(bans.remove(first).map(|b| b.identity), Some(ALICE));
        assert_eq!(bans.remove(first), None);
        assert!(bans.find(ALICE, ip("10.0.0.1"), 100).is_none());
        assert_eq!(bans.add(ban(ALICE, None, None), 100).id, second + 1, "ids are not reused while in the list");
    }

    #[test]
    fn saved_bans_load_back_without_the_expired_ones() {
        let dir = std::env::temp_dir().join(format!("t7-bans-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bans.toml");
        let mut bans = Bans::default();
        bans.add(ban(ALICE, Some(ip("10.0.0.1")), None), 100);
        bans.add(ban(BOB, None, Some(50)), 100);
        bans.save(&file).unwrap();

        let back = Bans::load(&file, 120).unwrap();
        assert_eq!(back.list(), bans.list());
        let later = Bans::load(&file, 150).unwrap();
        assert_eq!(later.list().iter().map(|b| b.identity).collect::<Vec<_>>(), [ALICE]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(describe_duration(59), "less than a minute");
        assert_eq!(describe_duration(60), "1 minute");
        assert_eq!(describe_duration(3_599), "59 minutes");
        assert_eq!(describe_duration(86_400 * 7), "7 days");
    }
}
