//! The roles, who holds them and the pending ownership claim, and their saved
//! format. Pure state, no networking: who may change what is decided by the caller.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use t7_proto::Identity;
use t7_proto::perms::{Member, Perms, Role, RoleId, Roles};

/// `roles.toml`.
#[derive(Serialize, Deserialize)]
struct RolesFile {
    /// Makes whoever redeems it first the owner. Gone once redeemed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<Identity>,
    next_id: u32,
    everyone: Perms,
    /// Highest first.
    roles: Vec<Role>,
    #[serde(default)]
    members: BTreeMap<Identity, Member>,
}

pub struct RoleStore {
    roles: Roles,
    claim_token: Option<String>,
    /// Ids only count up, so a stale role list can't change a newer role.
    next_id: u32,
}

impl Default for RoleStore {
    fn default() -> Self {
        let roles = Roles::default();
        let next_id = roles.ranked.iter().map(|r| r.id.0).max().unwrap_or(0) + 1;
        Self { roles, claim_token: None, next_id }
    }
}

impl RoleStore {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let file: RolesFile = toml::from_str(&text).map_err(std::io::Error::other)?;
        Self::from_file(file).map_err(std::io::Error::other)
    }

    fn from_file(file: RolesFile) -> Result<Self, String> {
        let mut ids = HashSet::new();
        for role in &file.roles {
            if role.id == RoleId::EVERYONE || !ids.insert(role.id) {
                return Err(format!("role id {} is taken", role.id.0));
            }
        }
        if !ids.contains(&RoleId::MODERATOR) {
            return Err("the Moderator role (id 2) is missing: channel moderators hold it".into());
        }
        let highest = ids.iter().map(|r| r.0).max().unwrap_or(0);
        let owner = file.owner;
        let mut members = file.members;
        // Roles that no longer exist are dropped, and so is anyone left with none.
        for member in members.values_mut() {
            member.roles.retain(|r| ids.contains(r));
            member.roles.sort_unstable();
            member.roles.dedup();
        }
        members.retain(|identity, m| !m.roles.is_empty() || owner == Some(*identity));
        let roles = Roles { owner, ranked: file.roles, everyone: file.everyone, members };
        Ok(Self { roles, claim_token: file.claim_token, next_id: file.next_id.max(highest + 1) })
    }

    /// Write-then-rename, so a crash never truncates the file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let header = "# Roles, who holds them, and the owner, kept up to date by the server as\n\
                      # they change in T7. Edit it only while the server is stopped.\n\n";
        let file = RolesFile {
            claim_token: self.claim_token.clone(),
            owner: self.roles.owner,
            next_id: self.next_id,
            everyone: self.roles.everyone,
            roles: self.roles.ranked.clone(),
            members: self.roles.members.clone(),
        };
        let body = toml::to_string(&file).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        crate::write_secret(&tmp, format!("{header}{body}").as_bytes())?;
        std::fs::rename(&tmp, path)
    }

    pub fn roles(&self) -> &Roles {
        &self.roles
    }

    pub fn claim_token(&self) -> Option<&str> {
        self.claim_token.as_deref()
    }

    /// Replaces any pending claim token. The owner stays until it is redeemed.
    pub fn set_claim_token(&mut self, token: String) {
        self.claim_token = Some(token);
    }

    /// Makes `identity` the owner if `token` is the pending claim token, which
    /// it uses up. A previous owner is no longer one.
    pub fn claim(&mut self, token: &str, identity: Identity, nickname: &str) -> bool {
        if self.claim_token.as_deref() != Some(token.trim()) {
            return false;
        }
        self.claim_token = None;
        let previous = self.roles.owner.replace(identity);
        if let Some(previous) = previous.filter(|p| *p != identity) {
            self.roles.members.retain(|i, m| *i != previous || !m.roles.is_empty());
        }
        let member =
            self.roles.members.entry(identity).or_insert(Member { nickname: String::new(), roles: Vec::new() });
        nickname.clone_into(&mut member.nickname);
        true
    }

    /// Remembers the nickname of someone who holds roles. Returns whether it changed.
    pub fn seen(&mut self, identity: Identity, nickname: &str) -> bool {
        match self.roles.members.get_mut(&identity) {
            Some(m) if m.nickname != nickname => {
                nickname.clone_into(&mut m.nickname);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: Identity = Identity([1; 32]);
    const BOB: Identity = Identity([2; 32]);

    fn temp_file(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("t7-roles-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("roles.toml")
    }

    #[test]
    fn a_claim_token_works_once_and_moves_ownership() {
        let mut store = RoleStore::default();
        assert!(!store.claim("", ALICE, "alice"), "no token, no claim");
        store.set_claim_token("abc".into());
        assert!(!store.claim("abd", ALICE, "alice"));
        assert!(store.claim(" abc ", ALICE, "alice"));
        assert_eq!((store.roles().owner, store.claim_token()), (Some(ALICE), None));
        assert!(!store.claim("abc", BOB, "bob"), "used up");

        // A reset token hands it to someone else.
        store.set_claim_token("def".into());
        assert_eq!(store.roles().owner, Some(ALICE), "until it is redeemed");
        assert!(store.claim("def", BOB, "bob"));
        assert_eq!(store.roles().owner, Some(BOB));
        assert!(!store.roles().members.contains_key(&ALICE), "the old owner held no roles");
    }

    #[test]
    fn everything_survives_a_restart() {
        let file = temp_file("restart");
        let mut store = RoleStore::default();
        store.set_claim_token("abc".into());
        store.claim("abc", ALICE, "alice");
        store.set_claim_token("pending".into());
        assert!(store.seen(ALICE, "Alice"));
        assert!(!store.seen(ALICE, "Alice"));
        assert!(!store.seen(BOB, "bob"), "only people with roles are remembered");
        store.save(&file).unwrap();

        let back = RoleStore::load(&file).unwrap();
        assert_eq!(back.roles(), store.roles());
        assert_eq!(back.claim_token(), Some("pending"));
        assert_eq!(back.roles().members[&ALICE].nickname, "Alice");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn hand_edits_are_checked_on_load() {
        let file = |extra: &str| {
            let text = format!(
                "next_id = 3\neveryone = [\"view\"]\n{extra}\n[[roles]]\nid = 2\nname = \"Moderator\"\nperms = []\n"
            );
            RoleStore::from_file(toml::from_str(&text).unwrap())
        };
        let bob = BOB.to_hex();
        let store = file(&format!("[members.{bob}]\nnickname = \"bob\"\nroles = [2, 7, 2]\n")).unwrap();
        assert_eq!(store.roles().members[&BOB].roles, [RoleId::MODERATOR], "unknown and repeated roles go");
        let store = file(&format!("[members.{bob}]\nnickname = \"bob\"\nroles = [7]\n")).unwrap();
        assert!(store.roles().members.is_empty(), "so does someone left with none");

        let dup = "[[roles]]\nid = 2\nname = \"Twin\"\nperms = []\n";
        assert!(file(dup).is_err());
        let text = "next_id = 3\neveryone = []\nroles = []\n";
        assert!(RoleStore::from_file(toml::from_str(text).unwrap()).is_err(), "the Moderator role must stay");
    }
}
