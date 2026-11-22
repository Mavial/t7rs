use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use t7_proto::perms::{Member, Perms, Role, RoleId, Roles};
use t7_proto::{Identity, MAX_ROLE_NAME_LEN, MAX_ROLES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleError {
    NotFound,
    BadName,
    NameTaken,
    TooMany,
    Everyone,
    Moderator,
}

impl RoleError {
    pub fn message(self) -> String {
        match self {
            RoleError::NotFound => "that role does not exist".into(),
            RoleError::BadName => format!("role names need 1 to {MAX_ROLE_NAME_LEN} characters"),
            RoleError::NameTaken => "a role with that name already exists".into(),
            RoleError::TooMany => format!("the server has the maximum of {MAX_ROLES} roles"),
            RoleError::Everyone => "everyone has the Everyone role; only its permissions change".into(),
            RoleError::Moderator => "the Moderator role can't be deleted: channel moderators hold it".into(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct RolesFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<Identity>,
    next_id: u32,
    everyone: Perms,
    roles: Vec<Role>,
    #[serde(default)]
    members: BTreeMap<Identity, Member>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignment {
    Changed,
    Unchanged,
}

pub struct RoleStore {
    roles: Roles,
    claim_token: Option<String>,
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
        let mut members = file.members;
        for member in members.values_mut() {
            member.roles.retain(|r| ids.contains(r));
            member.roles.sort_unstable();
            member.roles.dedup();
        }
        let mut roles = Roles { owner: file.owner, ranked: file.roles, everyone: file.everyone, members };
        prune_members(&mut roles);
        Ok(Self { roles, claim_token: file.claim_token, next_id: file.next_id.max(highest + 1) })
    }

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
        crate::write_atomic(path, |tmp| crate::write_secret(tmp, format!("{header}{body}").as_bytes()))
    }

    pub fn roles(&self) -> &Roles {
        &self.roles
    }

    pub fn claim_token(&self) -> Option<&str> {
        self.claim_token.as_deref()
    }

    pub fn set_claim_token(&mut self, token: String) {
        self.claim_token = Some(token);
    }

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

    pub fn create(&mut self, name: &str) -> Result<RoleId, RoleError> {
        if self.roles.ranked.len() >= MAX_ROLES {
            return Err(RoleError::TooMany);
        }
        let name = self.normalized_name(name, None)?;
        let id = RoleId(self.next_id);
        self.next_id += 1;
        self.roles.ranked.push(Role { id, name, perms: Perms::NONE });
        Ok(id)
    }

    pub fn rename(&mut self, id: RoleId, name: &str) -> Result<(), RoleError> {
        let name = self.normalized_name(name, Some(id))?;
        self.role_mut(id)?.name = name;
        Ok(())
    }

    pub fn set_perms(&mut self, id: RoleId, perms: Perms) -> Result<(), RoleError> {
        if id == RoleId::EVERYONE {
            self.roles.everyone = perms;
        } else {
            self.role_mut(id)?.perms = perms;
        }
        Ok(())
    }

    pub fn move_to(&mut self, id: RoleId, position: usize) -> Result<(), RoleError> {
        let at = self.index(id)?;
        let role = self.roles.ranked.remove(at);
        let position = position.min(self.roles.ranked.len());
        self.roles.ranked.insert(position, role);
        Ok(())
    }

    pub fn delete(&mut self, id: RoleId) -> Result<(), RoleError> {
        if id == RoleId::MODERATOR {
            return Err(RoleError::Moderator);
        }
        let at = self.index(id)?;
        self.roles.ranked.remove(at);
        for member in self.roles.members.values_mut() {
            member.roles.retain(|r| *r != id);
        }
        prune_members(&mut self.roles);
        Ok(())
    }

    pub fn grant(&mut self, identity: Identity, nickname: &str, role: RoleId) -> Result<Assignment, RoleError> {
        self.index(role)?;
        let member =
            self.roles.members.entry(identity).or_insert(Member { nickname: String::new(), roles: Vec::new() });
        nickname.clone_into(&mut member.nickname);
        if member.roles.contains(&role) {
            return Ok(Assignment::Unchanged);
        }
        member.roles.push(role);
        member.roles.sort_unstable();
        Ok(Assignment::Changed)
    }

    pub fn revoke(&mut self, identity: Identity, role: RoleId) -> Result<Assignment, RoleError> {
        self.index(role)?;
        let Some(member) = self.roles.members.get_mut(&identity) else { return Ok(Assignment::Unchanged) };
        if !member.roles.contains(&role) {
            return Ok(Assignment::Unchanged);
        }
        member.roles.retain(|r| *r != role);
        if member.roles.is_empty() && self.roles.owner != Some(identity) {
            self.roles.members.remove(&identity);
        }
        Ok(Assignment::Changed)
    }

    fn index(&self, id: RoleId) -> Result<usize, RoleError> {
        if id == RoleId::EVERYONE {
            return Err(RoleError::Everyone);
        }
        self.roles.ranked.iter().position(|r| r.id == id).ok_or(RoleError::NotFound)
    }

    fn role_mut(&mut self, id: RoleId) -> Result<&mut Role, RoleError> {
        let at = self.index(id)?;
        Ok(&mut self.roles.ranked[at])
    }

    fn normalized_name(&self, raw: &str, except: Option<RoleId>) -> Result<String, RoleError> {
        let name: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
        if name.is_empty() || name.chars().count() > MAX_ROLE_NAME_LEN {
            return Err(RoleError::BadName);
        }
        let reserved = ["Everyone", "Owner"].iter().any(|r| r.eq_ignore_ascii_case(&name));
        let taken = self.roles.ranked.iter().any(|r| Some(r.id) != except && r.name.eq_ignore_ascii_case(&name));
        if reserved || taken {
            return Err(RoleError::NameTaken);
        }
        Ok(name)
    }

    pub fn refresh_nickname(&mut self, identity: Identity, nickname: &str) -> bool {
        match self.roles.members.get_mut(&identity) {
            Some(m) if m.nickname != nickname => {
                nickname.clone_into(&mut m.nickname);
                true
            }
            _ => false,
        }
    }
}

fn prune_members(roles: &mut Roles) {
    let owner = roles.owner;
    roles.members.retain(|identity, m| !m.roles.is_empty() || owner == Some(*identity));
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

        store.set_claim_token("def".into());
        assert_eq!(store.roles().owner, Some(ALICE), "until it is redeemed");
        assert!(store.claim("def", BOB, "bob"));
        assert_eq!(store.roles().owner, Some(BOB));
        assert!(!store.roles().members.contains_key(&ALICE), "the old owner held no roles");
    }

    #[test]
    fn roles_are_made_ordered_and_removed() {
        let mut store = RoleStore::default();
        let dj = store.create("  DJ ").unwrap();
        assert_eq!(store.roles().role(dj).unwrap().name, "DJ");
        assert_eq!(store.roles().ranked.last().unwrap().id, dj, "new roles start at the bottom");
        for name in ["dj", "", "everyone", "OWNER", &"x".repeat(MAX_ROLE_NAME_LEN + 1)] {
            assert!(store.create(name).is_err(), "{name:?}");
        }
        assert_eq!(store.rename(RoleId::ADMIN, "Moderator"), Err(RoleError::NameTaken));
        store.rename(dj, "Dj").unwrap();

        store.move_to(dj, 0).unwrap();
        assert_eq!(store.roles().ranked[0].id, dj);
        store.move_to(dj, 99).unwrap();
        assert_eq!(store.roles().ranked.last().unwrap().id, dj);
        assert_eq!(store.move_to(RoleId::EVERYONE, 0), Err(RoleError::Everyone));

        let perms = Perms::of(&[t7_proto::perms::Perm::Kick]);
        store.set_perms(dj, perms).unwrap();
        store.set_perms(RoleId::EVERYONE, perms).unwrap();
        assert_eq!((store.roles().role(dj).unwrap().perms, store.roles().everyone), (perms, perms));

        assert_eq!(store.delete(RoleId::MODERATOR), Err(RoleError::Moderator));
        assert_eq!(store.delete(RoleId::EVERYONE), Err(RoleError::Everyone));
        store.grant(ALICE, "alice", dj).unwrap();
        store.delete(dj).unwrap();
        assert!(store.roles().role(dj).is_none() && store.roles().members.is_empty(), "nobody holds it");
        assert_ne!(store.create("DJ").unwrap(), dj, "ids are not reused");

        for i in store.roles().ranked.len()..MAX_ROLES {
            store.create(&format!("r{i}")).unwrap();
        }
        assert_eq!(store.create("one more"), Err(RoleError::TooMany));
    }

    #[test]
    fn roles_are_given_and_taken() {
        let mut store = RoleStore::default();
        assert_eq!(store.grant(ALICE, "alice", RoleId::MODERATOR), Ok(Assignment::Changed));
        assert_eq!(store.grant(ALICE, "Alice", RoleId::MODERATOR), Ok(Assignment::Unchanged), "already held");
        assert_eq!(store.roles().members[&ALICE].nickname, "Alice", "the latest nickname");
        store.grant(ALICE, "Alice", RoleId::ADMIN).unwrap();
        assert_eq!(store.grant(ALICE, "Alice", RoleId::EVERYONE), Err(RoleError::Everyone));
        assert_eq!(store.grant(ALICE, "Alice", RoleId(99)), Err(RoleError::NotFound));

        assert_eq!(store.revoke(ALICE, RoleId::ADMIN), Ok(Assignment::Changed));
        assert_eq!(store.revoke(ALICE, RoleId::ADMIN), Ok(Assignment::Unchanged));
        assert_eq!(store.revoke(BOB, RoleId::ADMIN), Ok(Assignment::Unchanged), "never had it");
        store.revoke(ALICE, RoleId::MODERATOR).unwrap();
        assert!(store.roles().members.is_empty(), "forgotten with no roles left");

        store.set_claim_token("t".into());
        store.claim("t", BOB, "bob");
        store.grant(BOB, "bob", RoleId::ADMIN).unwrap();
        store.revoke(BOB, RoleId::ADMIN).unwrap();
        assert!(store.roles().members.contains_key(&BOB));
    }

    #[test]
    fn everything_survives_a_restart() {
        let file = temp_file("restart");
        let mut store = RoleStore::default();
        store.set_claim_token("abc".into());
        store.claim("abc", ALICE, "alice");
        store.set_claim_token("pending".into());
        assert!(store.refresh_nickname(ALICE, "Alice"));
        assert!(!store.refresh_nickname(ALICE, "Alice"));
        assert!(!store.refresh_nickname(BOB, "bob"), "only people with roles are remembered");
        let dj = store.create("DJ").unwrap();
        store.move_to(dj, 0).unwrap();
        store.grant(BOB, "bob", dj).unwrap();
        store.save(&file).unwrap();

        let back = RoleStore::load(&file).unwrap();
        assert_eq!(back.roles(), store.roles());
        assert_eq!(back.claim_token(), Some("pending"));
        assert_eq!(back.roles().members[&ALICE].nickname, "Alice");
        assert_ne!(RoleStore { next_id: back.next_id, ..RoleStore::default() }.create("New"), Ok(dj));
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
