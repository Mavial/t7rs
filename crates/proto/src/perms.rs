//! Who may do what: permission bits, the ordered roles that grant them, and
//! [`Roles::resolve`], the one function that decides what a person may do in a
//! channel. The server resolves and enforces; clients get their resolved bits
//! and use them only to decide what to offer.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign};

use serde::de::{self, Deserializer};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};

use crate::{ChannelId, ChannelInfo, ClientId, Identity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Perm {
    /// See the channel, its sub-channels and the people in them.
    View,
    Join,
    Speak,
    SendMessages,
    /// Join a channel that is full or has a password.
    Bypass,
    CreateTemporary,
    /// Create permanent channels, edit, delete and rearrange them.
    ManageChannels,
    /// Change channel permissions and moderator lists.
    ManagePermissions,
    MoveMembers,
    MuteMembers,
    /// Out of a channel, or off the server.
    Kick,
    /// Ban people and manage the ban list.
    Ban,
    /// Create, edit and hand out roles.
    ManageRoles,
}

impl Perm {
    pub const ALL: [Perm; 13] = [
        Perm::View,
        Perm::Join,
        Perm::Speak,
        Perm::SendMessages,
        Perm::Bypass,
        Perm::CreateTemporary,
        Perm::ManageChannels,
        Perm::ManagePermissions,
        Perm::MoveMembers,
        Perm::MuteMembers,
        Perm::Kick,
        Perm::Ban,
        Perm::ManageRoles,
    ];

    /// Its name in `roles.toml`.
    pub fn key(self) -> &'static str {
        match self {
            Perm::View => "view",
            Perm::Join => "join",
            Perm::Speak => "speak",
            Perm::SendMessages => "send_messages",
            Perm::Bypass => "bypass",
            Perm::CreateTemporary => "create_temporary",
            Perm::ManageChannels => "manage_channels",
            Perm::ManagePermissions => "manage_permissions",
            Perm::MoveMembers => "move_members",
            Perm::MuteMembers => "mute_members",
            Perm::Kick => "kick",
            Perm::Ban => "ban",
            Perm::ManageRoles => "manage_roles",
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Perms(u16);

impl Perms {
    pub const NONE: Perms = Perms(0);
    pub const ALL: Perms = Perms::of(&Perm::ALL);
    /// The ones a channel can set: those that mean something in one place.
    pub const CHANNEL: Perms = Perms(Perms::ALL.0 & !Perms::of(&[Perm::Ban, Perm::ManageRoles]).0);

    pub const fn of(list: &[Perm]) -> Perms {
        let mut bits = 0;
        let mut i = 0;
        while i < list.len() {
            bits |= list[i].bit();
            i += 1;
        }
        Perms(bits)
    }

    const fn from_bits_truncate(bits: u16) -> Perms {
        Perms(bits & Perms::ALL.0)
    }

    #[must_use]
    pub const fn union(self, other: Perms) -> Perms {
        Perms(self.0 | other.0)
    }

    pub fn has(self, perm: Perm) -> bool {
        self.0 & perm.bit() != 0
    }

    pub fn set(&mut self, perm: Perm, on: bool) {
        if on {
            self.0 |= perm.bit();
        } else {
            self.0 &= !perm.bit();
        }
    }

    #[must_use]
    pub fn without(self, other: Perms) -> Perms {
        Perms(self.0 & !other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Perm> {
        Perm::ALL.into_iter().filter(move |p| self.has(*p))
    }
}

impl BitOr for Perms {
    type Output = Perms;
    fn bitor(self, other: Perms) -> Perms {
        self.union(other)
    }
}

impl BitOrAssign for Perms {
    fn bitor_assign(&mut self, other: Perms) {
        self.0 |= other.0;
    }
}

impl BitAnd for Perms {
    type Output = Perms;
    fn bitand(self, other: Perms) -> Perms {
        Perms(self.0 & other.0)
    }
}

impl fmt::Debug for Perms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter().map(Perm::key)).finish()
    }
}

/// A list of names in text formats like TOML, the bits on the wire.
impl Serialize for Perms {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if !s.is_human_readable() {
            return self.0.serialize(s);
        }
        let mut seq = s.serialize_seq(None)?;
        for perm in self.iter() {
            seq.serialize_element(perm.key())?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Perms {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if !d.is_human_readable() {
            return u16::deserialize(d).map(Perms::from_bits_truncate);
        }
        let mut perms = Perms::NONE;
        for name in Vec::<String>::deserialize(d)? {
            let perm = Perm::ALL
                .into_iter()
                .find(|p| p.key() == name)
                .ok_or_else(|| de::Error::custom(format!("unknown permission {name:?}")))?;
            perms.set(perm, true);
        }
        Ok(perms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoleId(pub u32);

impl RoleId {
    /// Held by everyone, never assigned, and below every other role.
    pub const EVERYONE: RoleId = RoleId(0);
    pub const ADMIN: RoleId = RoleId(1);
    /// Also held by channel moderators where they moderate, and by the owner of
    /// a temporary channel in it.
    pub const MODERATOR: RoleId = RoleId(2);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: RoleId,
    pub name: String,
    pub perms: Perms,
}

/// Someone who owns the server or holds roles, remembered while they are offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Their nickname when last seen.
    pub nickname: String,
    pub roles: Vec<RoleId>,
}

/// Height in the role order. You may only act on people, and roles, below your own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    role_height: u32,
    room_owner: bool,
}

impl Rank {
    pub const EVERYONE: Rank = Rank::of_role(0);
    /// A room's owner, in it: above Everyone, below every role, so that
    /// moderators keep their say over people in rooms of their own.
    pub const ROOM_OWNER: Rank = Rank { role_height: 0, room_owner: true };
    pub const OWNER: Rank = Rank::of_role(u32::MAX);

    const fn of_role(role_height: u32) -> Rank {
        Rank { role_height, room_owner: false }
    }
}

const EVERYONE_PERMS: Perms =
    Perms::of(&[Perm::View, Perm::Join, Perm::Speak, Perm::SendMessages, Perm::CreateTemporary]);
const MODERATOR_PERMS: Perms =
    EVERYONE_PERMS.union(Perms::of(&[Perm::Bypass, Perm::MoveMembers, Perm::MuteMembers, Perm::Kick]));
const ADMIN_PERMS: Perms =
    MODERATOR_PERMS.union(Perms::of(&[Perm::ManageChannels, Perm::ManagePermissions, Perm::Ban, Perm::ManageRoles]));
const ROOM_OWNER_PERMS: Perms = Perms::of(&[Perm::ManageChannels]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roles {
    /// Above every role, with every permission.
    pub owner: Option<Identity>,
    /// Highest first. Everyone sits below them all.
    pub ranked: Vec<Role>,
    pub everyone: Perms,
    /// Everyone who owns the server or holds a role.
    pub members: BTreeMap<Identity, Member>,
}

impl Default for Roles {
    /// A fresh server's: Admin above Moderator, and no owner yet.
    fn default() -> Self {
        Self {
            owner: None,
            ranked: vec![
                Role { id: RoleId::ADMIN, name: "Admin".into(), perms: ADMIN_PERMS },
                Role { id: RoleId::MODERATOR, name: "Moderator".into(), perms: MODERATOR_PERMS },
            ],
            everyone: EVERYONE_PERMS,
            members: BTreeMap::new(),
        }
    }
}

/// A change to the roles, by someone who may manage them, to roles and
/// people below their own role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoleChange {
    /// A new role without permissions, at the bottom, just above Everyone.
    Create {
        name: String,
    },
    Rename {
        id: RoleId,
        name: String,
    },
    /// Everyone's too.
    SetPermissions {
        id: RoleId,
        perms: Perms,
    },
    /// `position` is the index among the roles, highest first.
    Move {
        id: RoleId,
        position: u32,
    },
    /// Whoever holds it loses it.
    Delete {
        id: RoleId,
    },
    /// By identity, so someone offline can lose a role. Only someone online can get one.
    Assign {
        identity: Identity,
        role: RoleId,
        held: bool,
    },
}

/// Who is asking: the person, and the connection, which may own a temporary channel.
#[derive(Debug, Clone, Copy)]
pub struct Subject {
    pub client: ClientId,
    pub identity: Identity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Server,
    Channel(ChannelId),
}

impl Scope {
    pub fn within(parent: Option<ChannelId>) -> Scope {
        parent.map_or(Scope::Server, Scope::Channel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    pub perms: Perms,
    /// Change a channel's name and topic. No role or channel holds it:
    /// [`Roles::resolve`] gives it in a channel to whoever may manage
    /// channels there, and to the channel's listed moderators.
    pub rename: bool,
    pub rank: Rank,
}

/// What a channel says about one permission for one role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Allow,
    Deny,
    /// Whatever the channel above says, or else the role itself.
    Inherit,
}

/// A channel's explicit settings for one role: each permission allowed,
/// denied, or (in neither) inherited from the channel above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "OverrideFields")]
pub struct Override {
    pub role: RoleId,
    allow: Perms,
    deny: Perms,
}

#[derive(Deserialize)]
struct OverrideFields {
    role: RoleId,
    #[serde(default)]
    allow: Perms,
    #[serde(default)]
    deny: Perms,
}

impl From<OverrideFields> for Override {
    fn from(f: OverrideFields) -> Self {
        Override::allowing(f.role, f.allow).and_denying(f.deny)
    }
}

impl Override {
    pub fn inheriting(role: RoleId) -> Self {
        Self { role, allow: Perms::NONE, deny: Perms::NONE }
    }

    pub fn allowing(role: RoleId, perms: Perms) -> Self {
        Self { role, allow: perms, deny: Perms::NONE }
    }

    pub fn denying(role: RoleId, perms: Perms) -> Self {
        Self { role, allow: Perms::NONE, deny: perms }
    }

    /// A permission it already allows stays allowed.
    #[must_use]
    pub fn and_denying(self, perms: Perms) -> Self {
        Self { deny: (self.deny | perms).without(self.allow), ..self }
    }

    pub fn allow(&self) -> Perms {
        self.allow
    }

    pub fn deny(&self) -> Perms {
        self.deny
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    pub fn get(&self, perm: Perm) -> Setting {
        if self.allow.has(perm) {
            Setting::Allow
        } else if self.deny.has(perm) {
            Setting::Deny
        } else {
            Setting::Inherit
        }
    }

    pub fn set(&mut self, perm: Perm, setting: Setting) {
        self.allow.set(perm, setting == Setting::Allow);
        self.deny.set(perm, setting == Setting::Deny);
    }
}

pub trait ChannelTree {
    fn channel(&self, id: ChannelId) -> Option<&ChannelInfo>;
    fn channels(&self) -> impl Iterator<Item = &ChannelInfo>;
}

fn self_then_ancestors(scope: Scope, tree: &impl ChannelTree) -> Vec<&ChannelInfo> {
    let mut path = Vec::new();
    let mut at = match scope {
        Scope::Server => None,
        Scope::Channel(id) => tree.channel(id),
    };
    while let Some(c) = at {
        path.push(c);
        at = c.parent.and_then(|p| tree.channel(p));
    }
    path
}

impl Roles {
    pub fn role(&self, id: RoleId) -> Option<&Role> {
        self.ranked.iter().find(|r| r.id == id)
    }

    pub fn height(&self, role: RoleId) -> Rank {
        self.ranked.iter().position(|r| r.id == role).map_or(Rank::EVERYONE, |i| self.height_at(i))
    }

    /// The height of the role at `position` among them, highest first, or of
    /// one moved there.
    pub fn height_at(&self, position: usize) -> Rank {
        let len = self.ranked.len();
        Rank::of_role((len - position.min(len.saturating_sub(1))) as u32)
    }

    /// Their highest standing on the server, wherever they are.
    pub fn rank(&self, identity: Identity) -> Rank {
        if self.owner == Some(identity) {
            return Rank::OWNER;
        }
        self.assigned(identity).iter().map(|r| self.height(*r)).max().unwrap_or(Rank::EVERYONE)
    }

    /// What the channel tree shows next to their name: their highest role.
    pub fn title(&self, identity: Identity) -> Option<String> {
        if self.owner == Some(identity) {
            return Some("Owner".into());
        }
        let held = self.assigned(identity);
        self.ranked.iter().find(|r| held.contains(&r.id)).map(|r| r.name.clone())
    }

    fn assigned(&self, identity: Identity) -> &[RoleId] {
        self.members.get(&identity).map_or(&[], |m| &m.roles)
    }

    fn base(&self, role: RoleId) -> Perms {
        if role == RoleId::EVERYONE {
            return self.everyone;
        }
        self.role(role).map_or(Perms::NONE, |r| r.perms)
    }

    /// What `who` may do in a channel, or on the server as a whole.
    ///
    /// The owner may do everything. Anyone else has what their roles grant,
    /// Everyone's included. In a channel, and every channel below it, its
    /// moderators also hold the Moderator role, and so does the owner of a
    /// temporary channel, though ranked just above Everyone. Then each permission a channel sets for one of their roles
    /// overrides what the roles grant; the closest channel that sets it wins,
    /// and there an allow beats a deny. In a channel, its moderators and
    /// whoever may manage it may rename it.
    pub fn resolve(&self, who: Subject, scope: Scope, tree: &impl ChannelTree) -> Standing {
        let path = self_then_ancestors(scope, tree);
        let moderates = path.iter().any(|c| c.moderators.contains(&who.identity));
        let (perms, rank) = if self.owner == Some(who.identity) {
            (Perms::ALL, Rank::OWNER)
        } else {
            let owns_room = path.iter().any(|c| c.temporary && c.owner == Some(who.client));
            let mut held = vec![RoleId::EVERYONE];
            held.extend_from_slice(self.assigned(who.identity));
            if moderates {
                held.push(RoleId::MODERATOR);
            }
            let mut rank = held.iter().map(|r| self.height(*r)).max().unwrap_or(Rank::EVERYONE);
            if owns_room {
                held.push(RoleId::MODERATOR);
                rank = rank.max(Rank::ROOM_OWNER);
            }
            let mut perms = self.grant(&held, &path);
            if owns_room {
                perms |= ROOM_OWNER_PERMS;
            }
            (perms, rank)
        };
        let rename = !path.is_empty() && (moderates || perms.has(Perm::ManageChannels));
        Standing { perms, rename, rank }
    }

    fn grant(&self, held: &[RoleId], path: &[&ChannelInfo]) -> Perms {
        let mut perms = held.iter().fold(Perms::NONE, |p, r| p | self.base(*r));
        let mut settled = Perms::NONE;
        for c in path {
            let (mut allow, mut deny) = (Perms::NONE, Perms::NONE);
            for o in c.overrides.iter().filter(|o| held.contains(&o.role)) {
                allow |= o.allow;
                deny |= o.deny;
            }
            let allow = allow.without(settled);
            let deny = deny.without(allow).without(settled);
            perms = (perms | allow).without(deny);
            settled = settled | allow | deny;
        }
        perms
    }

    /// What someone with no role may do in `channel`.
    pub fn everyone_in(&self, channel: ChannelId, tree: &impl ChannelTree) -> Perms {
        self.grant(&[RoleId::EVERYONE], &self_then_ancestors(Scope::Channel(channel), tree))
    }

    /// What someone holding just `role` would get in `channel`, as given, if
    /// it set nothing for that role: what an Inherit there stands for.
    pub fn inherited(&self, role: RoleId, channel: &ChannelInfo, tree: &impl ChannelTree) -> Perms {
        let mut here = channel.clone();
        here.overrides.retain(|o| o.role != role);
        let mut path = self_then_ancestors(Scope::within(here.parent), tree);
        path.insert(0, &here);
        let held = if role == RoleId::EVERYONE { vec![role] } else { vec![RoleId::EVERYONE, role] };
        self.grant(&held, &path)
    }

    /// What `who` may do on the server, and in each channel they see. They
    /// see a channel they may view or manage, if they see the one above it:
    /// a hidden channel hides everything below it.
    pub fn permissions(&self, who: Subject, tree: &impl ChannelTree) -> Permissions {
        let all: BTreeMap<ChannelId, ChannelPerms> = tree
            .channels()
            .map(|c| {
                let Standing { perms, rename, .. } = self.resolve(who, Scope::Channel(c.id), tree);
                (c.id, ChannelPerms { perms, rename })
            })
            .collect();
        let shows = |id| all.get(&id).is_some_and(|p| p.perms.has(Perm::View) || p.perms.has(Perm::ManageChannels));
        let sees = |c: &ChannelInfo| self_then_ancestors(Scope::Channel(c.id), tree).iter().all(|c| shows(c.id));
        let channels = tree.channels().filter(|c| sees(c)).map(|c| (c.id, all[&c.id])).collect();
        Permissions { server: self.resolve(who, Scope::Server, tree).perms, channels }
    }
}

/// What one client may do: on the server as a whole, and in each channel it sees.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Permissions {
    pub server: Perms,
    /// Exactly the channels it sees.
    pub channels: BTreeMap<ChannelId, ChannelPerms>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "u16", into = "u16")]
pub struct ChannelPerms {
    pub perms: Perms,
    pub rename: bool,
}

const RENAME_BIT: u16 = 1 << Perm::ALL.len();

impl From<ChannelPerms> for u16 {
    fn from(c: ChannelPerms) -> u16 {
        if c.rename { c.perms.0 | RENAME_BIT } else { c.perms.0 }
    }
}

impl From<u16> for ChannelPerms {
    fn from(bits: u16) -> Self {
        ChannelPerms { perms: Perms::from_bits_truncate(bits), rename: bits & RENAME_BIT != 0 }
    }
}

impl Permissions {
    pub fn sees(&self, channel: ChannelId) -> bool {
        self.channels.contains_key(&channel)
    }

    pub fn may_in(&self, channel: ChannelId, perm: Perm) -> bool {
        self.channels.get(&channel).is_some_and(|p| p.perms.has(perm))
    }

    pub fn may_rename(&self, channel: ChannelId) -> bool {
        self.channels.get(&channel).is_some_and(|p| p.rename)
    }

    /// Whether it gets every role and what each channel sets for them, to
    /// edit those: it may manage roles, or some channel's permissions.
    pub fn edits_permissions(&self) -> bool {
        let edits = |p: &Perms| p.has(Perm::ManageRoles) || p.has(Perm::ManagePermissions);
        edits(&self.server) || self.channels.values().any(|c| edits(&c.perms))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::ChannelKind;

    // Lobby (default)
    // Gaming            moderated by M
    //   Squad
    //     Deep
    // AFK
    // Raid (temporary, made by client 9)
    const LOBBY: ChannelId = 1;
    const GAMING: ChannelId = 2;
    const SQUAD: ChannelId = 3;
    const DEEP: ChannelId = 4;
    const AFK: ChannelId = 5;
    const RAID: ChannelId = 6;
    const OWNER: Identity = Identity([0xf; 32]);
    const ADMIN: Identity = Identity([0xa; 32]);
    const MODERATOR: Identity = Identity([0xd; 32]);
    const M: Identity = Identity([0xb; 32]);
    const NOBODY: Identity = Identity([0xc; 32]);

    struct Tree {
        channels: HashMap<ChannelId, ChannelInfo>,
    }

    impl Tree {
        fn set(&mut self, channel: ChannelId, overrides: Vec<Override>) {
            self.channels.get_mut(&channel).unwrap().overrides = overrides;
        }
    }

    impl ChannelTree for Tree {
        fn channel(&self, id: ChannelId) -> Option<&ChannelInfo> {
            self.channels.get(&id)
        }
        fn channels(&self) -> impl Iterator<Item = &ChannelInfo> {
            self.channels.values()
        }
    }

    fn tree() -> Tree {
        let ch = |id, parent, moderators: &[Identity]| ChannelInfo {
            id,
            parent,
            name: format!("c{id}"),
            topic: String::new(),
            max_clients: None,
            order: 0,
            temporary: false,
            has_password: false,
            default: id == LOBBY,
            owner: None,
            moderators: moderators.to_vec(),
            kind: ChannelKind::Voice,
            overrides: Vec::new(),
        };
        let raid = ChannelInfo { temporary: true, owner: Some(9), ..ch(RAID, None, &[]) };
        let channels = [
            ch(LOBBY, None, &[]),
            ch(GAMING, None, &[M]),
            ch(SQUAD, Some(GAMING), &[]),
            ch(DEEP, Some(SQUAD), &[]),
            ch(AFK, None, &[]),
            raid,
        ];
        Tree { channels: channels.into_iter().map(|c| (c.id, c)).collect() }
    }

    fn roles() -> Roles {
        let mut roles = Roles { owner: Some(OWNER), ..Roles::default() };
        for (identity, role) in [(ADMIN, RoleId::ADMIN), (MODERATOR, RoleId::MODERATOR)] {
            roles.members.insert(identity, Member { nickname: "x".into(), roles: vec![role] });
        }
        roles
    }

    fn who(identity: Identity) -> Subject {
        Subject { client: 1, identity }
    }

    fn perms(roles: &Roles, tree: &Tree, identity: Identity, scope: Scope) -> Perms {
        roles.resolve(who(identity), scope, tree).perms
    }

    #[test]
    fn bits_are_names_in_files_and_bits_on_the_wire() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct File {
            perms: Perms,
        }
        let file = File { perms: Perms::of(&[Perm::Kick, Perm::View]) };
        let text = toml::to_string(&file).unwrap();
        assert_eq!(text.trim(), r#"perms = ["view", "kick"]"#);
        assert_eq!(toml::from_str::<File>(&text).unwrap(), file);
        assert!(toml::from_str::<File>(r#"perms = ["fly"]"#).is_err());

        let wire = postcard::to_allocvec(&file.perms).unwrap();
        assert_eq!(postcard::from_bytes::<Perms>(&wire).unwrap(), file.perms);
        let unknown = postcard::to_allocvec(&u16::MAX).unwrap();
        assert_eq!(postcard::from_bytes::<Perms>(&unknown).unwrap(), Perms::ALL, "unknown bits are dropped");

        let renames = ChannelPerms { perms: file.perms, rename: true };
        let wire = postcard::to_allocvec(&renames).unwrap();
        assert_eq!(postcard::from_bytes::<ChannelPerms>(&wire).unwrap(), renames);
        let everything = ChannelPerms { perms: Perms::ALL, rename: true };
        assert_eq!(postcard::from_bytes::<ChannelPerms>(&unknown).unwrap(), everything, "unknown bits are dropped");
    }

    #[test]
    fn a_fresh_server_has_the_default_roles() {
        let (roles, t) = (roles(), tree());
        let everyone = Perms::of(&[Perm::View, Perm::Join, Perm::Speak, Perm::SendMessages, Perm::CreateTemporary]);
        assert_eq!(perms(&roles, &t, NOBODY, Scope::Server), everyone);
        let moderator = everyone | Perms::of(&[Perm::Bypass, Perm::MoveMembers, Perm::MuteMembers, Perm::Kick]);
        assert_eq!(perms(&roles, &t, MODERATOR, Scope::Server), moderator);
        assert_eq!(perms(&roles, &t, ADMIN, Scope::Server), Perms::ALL);
        assert_eq!(perms(&roles, &t, OWNER, Scope::Channel(AFK)), Perms::ALL);
    }

    #[test]
    fn rank_follows_the_role_order() {
        let mut roles = roles();
        assert!(roles.rank(OWNER) > roles.rank(ADMIN));
        assert!(roles.rank(ADMIN) > roles.rank(MODERATOR));
        assert!(roles.rank(MODERATOR) > roles.rank(NOBODY));
        assert_eq!(roles.rank(NOBODY), Rank::EVERYONE);
        assert_eq!(roles.height(RoleId::EVERYONE), Rank::EVERYONE);
        assert_eq!(
            [OWNER, ADMIN, MODERATOR, NOBODY].map(|i| roles.title(i)),
            [Some("Owner".into()), Some("Admin".into()), Some("Moderator".into()), None]
        );

        roles.members.get_mut(&MODERATOR).unwrap().roles.push(RoleId::ADMIN);
        assert_eq!(roles.rank(MODERATOR), roles.rank(ADMIN));
        roles.ranked.reverse();
        roles.members.get_mut(&MODERATOR).unwrap().roles = vec![RoleId::MODERATOR];
        assert!(roles.rank(MODERATOR) > roles.rank(ADMIN));
    }

    #[test]
    fn channel_moderators_hold_the_moderator_role_down_their_tree() {
        let (roles, t) = (roles(), tree());
        let moderator = roles.role(RoleId::MODERATOR).unwrap().perms;
        for channel in [GAMING, SQUAD, DEEP] {
            let standing = roles.resolve(who(M), Scope::Channel(channel), &t);
            assert_eq!(
                standing,
                Standing { perms: moderator, rename: true, rank: roles.height(RoleId::MODERATOR) },
                "{channel}"
            );
        }
        for scope in [Scope::Server, Scope::Channel(LOBBY), Scope::Channel(AFK)] {
            assert_eq!(roles.resolve(who(M), scope, &t).rank, Rank::EVERYONE, "{scope:?}");
            assert!(!perms(&roles, &t, M, scope).has(Perm::Kick), "{scope:?}");
        }
        assert_eq!(
            roles.resolve(who(ADMIN), Scope::Channel(GAMING), &t).rank,
            roles.rank(ADMIN),
            "an admin stays an admin"
        );
    }

    #[test]
    fn listed_moderators_and_channel_managers_rename_channels() {
        let (roles, t) = (roles(), tree());
        let renames = |identity, scope| roles.resolve(who(identity), scope, &t).rename;
        assert!(
            renames(M, Scope::Channel(GAMING)) && renames(M, Scope::Channel(DEEP)),
            "where they are listed, and below"
        );
        assert!(!renames(M, Scope::Channel(LOBBY)), "elsewhere");
        let listed = perms(&roles, &t, M, Scope::Channel(GAMING));
        assert!(!listed.has(Perm::ManageChannels) && !listed.has(Perm::ManagePermissions), "nothing more");
        assert!(!renames(MODERATOR, Scope::Channel(GAMING)), "holding the role isn't being listed");
        assert!(renames(ADMIN, Scope::Channel(LOBBY)) && renames(OWNER, Scope::Channel(LOBBY)));
        assert!(!renames(ADMIN, Scope::Server), "only a channel is renamed");
        let room_owner = Subject { client: 9, identity: NOBODY };
        assert!(roles.resolve(room_owner, Scope::Channel(RAID), &t).rename, "a room's owner manages it");
        assert!(toml::from_str::<Role>("id = 7\nname = \"x\"\nperms = [\"rename\"]").is_err());
    }

    #[test]
    fn room_owners_moderate_and_manage_their_room() {
        let (roles, t) = (roles(), tree());
        let room_owner = Subject { client: 9, identity: NOBODY };
        let room = roles.resolve(room_owner, Scope::Channel(RAID), &t);
        assert!(room.perms.has(Perm::Kick) && room.perms.has(Perm::ManageChannels));
        assert!(!room.perms.has(Perm::Ban));
        assert!(room.rank > Rank::EVERYONE && room.rank < roles.height(RoleId::MODERATOR));
        let mut lowest = roles.clone();
        lowest.ranked.push(Role { id: RoleId(7), name: "DJ".into(), perms: Perms::NONE });
        assert!(room.rank < lowest.height(RoleId(7)));
        let moderator = Subject { client: 9, identity: MODERATOR };
        assert_eq!(
            roles.resolve(moderator, Scope::Channel(RAID), &t).rank,
            roles.height(RoleId::MODERATOR),
            "a role counts"
        );
        assert!(!roles.resolve(room_owner, Scope::Channel(LOBBY), &t).perms.has(Perm::Kick), "elsewhere");
        let other = Subject { client: 8, identity: NOBODY };
        assert!(!roles.resolve(other, Scope::Channel(RAID), &t).perms.has(Perm::Kick), "another connection");
    }

    #[test]
    fn the_closest_channel_setting_wins_and_there_allow_beats_deny() {
        let (roles, mut t) = (roles(), tree());
        let (view, speak) = (Perms::of(&[Perm::View]), Perms::of(&[Perm::Speak]));
        t.set(GAMING, vec![Override::denying(RoleId::EVERYONE, view | speak)]);
        t.set(SQUAD, vec![Override::allowing(RoleId::EVERYONE, speak)]);
        t.set(DEEP, vec![Override::allowing(RoleId::MODERATOR, view)]);

        assert!(!perms(&roles, &t, NOBODY, Scope::Channel(GAMING)).has(Perm::View));
        assert!(!perms(&roles, &t, NOBODY, Scope::Channel(SQUAD)).has(Perm::View), "inherited from Gaming");
        assert!(perms(&roles, &t, NOBODY, Scope::Channel(SQUAD)).has(Perm::Speak), "Squad's own setting is closer");
        assert!(perms(&roles, &t, NOBODY, Scope::Channel(LOBBY)).has(Perm::View), "elsewhere");
        assert!(perms(&roles, &t, MODERATOR, Scope::Channel(DEEP)).has(Perm::View), "the moderators' allow is closer");
        assert!(!perms(&roles, &t, NOBODY, Scope::Channel(DEEP)).has(Perm::View));

        t.set(GAMING, vec![Override::denying(RoleId::EVERYONE, view), Override::allowing(RoleId::MODERATOR, view)]);
        assert!(perms(&roles, &t, MODERATOR, Scope::Channel(GAMING)).has(Perm::View));
        assert!(perms(&roles, &t, M, Scope::Channel(SQUAD)).has(Perm::View), "listed moderators hold the role there");
        assert!(!perms(&roles, &t, NOBODY, Scope::Channel(GAMING)).has(Perm::View));
        assert_eq!(perms(&roles, &t, OWNER, Scope::Channel(GAMING)), Perms::ALL, "nothing overrides the owner");
        assert_eq!(perms(&roles, &t, NOBODY, Scope::Server), roles.everyone, "nor applies to the server as a whole");
    }

    #[test]
    fn overrides_are_tri_state_and_allow_wins_within_one() {
        let mut o = Override::allowing(RoleId::EVERYONE, Perms::of(&[Perm::View]))
            .and_denying(Perms::of(&[Perm::View, Perm::Speak]));
        assert_eq!(
            (o.get(Perm::View), o.get(Perm::Speak), o.get(Perm::Join)),
            (Setting::Allow, Setting::Deny, Setting::Inherit)
        );
        o.set(Perm::View, Setting::Deny);
        o.set(Perm::Speak, Setting::Inherit);
        assert_eq!((o.allow(), o.deny()), (Perms::NONE, Perms::of(&[Perm::View])));
        o.set(Perm::View, Setting::Inherit);
        assert!(o.is_empty());

        let file = "role = 2\nallow = [\"kick\"]\ndeny = [\"kick\", \"view\"]\n";
        let read: Override = toml::from_str(file).unwrap();
        assert_eq!(
            read,
            Override::allowing(RoleId::MODERATOR, Perms::of(&[Perm::Kick])).and_denying(Perms::of(&[Perm::View]))
        );
        assert_eq!(toml::from_str::<Override>("role = 0\ndeny = [\"view\"]").unwrap().get(Perm::View), Setting::Deny);
        let wire = postcard::to_allocvec(&read).unwrap();
        assert_eq!(postcard::from_bytes::<Override>(&wire).unwrap(), read);
    }

    #[test]
    fn a_hidden_channel_hides_what_is_below_it_but_not_from_managers() {
        let (roles, mut t) = (roles(), tree());
        let view = Perms::of(&[Perm::View]);
        t.set(GAMING, vec![Override::denying(RoleId::EVERYONE, view)]);
        t.set(SQUAD, vec![Override::allowing(RoleId::EVERYONE, view)]);
        let seen = |t: &Tree, identity| roles.permissions(who(identity), t).channels.into_keys().collect::<Vec<_>>();
        assert_eq!(seen(&t, NOBODY), [LOBBY, AFK, RAID]);
        assert_eq!(seen(&t, MODERATOR), [LOBBY, AFK, RAID]);
        assert_eq!(seen(&t, ADMIN), [LOBBY, GAMING, SQUAD, DEEP, AFK, RAID]);
        assert!(!roles.permissions(who(ADMIN), &t).may_in(GAMING, Perm::View));
        t.set(GAMING, vec![Override::denying(RoleId::EVERYONE, view), Override::allowing(RoleId::MODERATOR, view)]);
        assert_eq!(seen(&t, M), [LOBBY, GAMING, SQUAD, DEEP, AFK, RAID]);
        assert_eq!(seen(&t, NOBODY), [LOBBY, AFK, RAID]);

        let p = roles.permissions(who(NOBODY), &t);
        assert!(!p.edits_permissions() && roles.permissions(who(ADMIN), &t).edits_permissions());
        assert!(p.may_in(LOBBY, Perm::Join) && !p.may_in(GAMING, Perm::Join) && !p.sees(99));
    }

    #[test]
    fn inherit_stands_for_the_channel_above_and_the_roles_held_with_it() {
        let (mut roles, mut t) = (roles(), tree());
        let (view, speak) = (Perms::of(&[Perm::View]), Perms::of(&[Perm::Speak]));
        roles.everyone = roles.everyone.without(speak);
        t.set(GAMING, vec![Override::allowing(RoleId::EVERYONE, speak).and_denying(view)]);
        t.set(SQUAD, vec![Override::denying(RoleId::EVERYONE, speak)]);

        let squad = t.channel(SQUAD).unwrap().clone();
        let inherited = roles.inherited(RoleId::EVERYONE, &squad, &t);
        assert!(inherited.has(Perm::Speak) && !inherited.has(Perm::View));
        assert!(!roles.everyone_in(SQUAD, &t).has(Perm::Speak), "its own setting");
        let moderator = roles.inherited(RoleId::MODERATOR, &squad, &t);
        assert!(!moderator.has(Perm::Speak) && moderator.has(Perm::Kick));
        let mut edited = squad;
        edited.overrides.clear();
        assert!(roles.inherited(RoleId::MODERATOR, &edited, &t).has(Perm::Speak));
    }
}
