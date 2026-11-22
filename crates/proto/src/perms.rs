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

/// One yes/no permission.
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

/// A set of permissions.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Perms(u16);

impl Perms {
    pub const NONE: Perms = Perms(0);
    pub const ALL: Perms = Perms::of(&Perm::ALL);

    pub const fn of(list: &[Perm]) -> Perms {
        let mut bits = 0;
        let mut i = 0;
        while i < list.len() {
            bits |= list[i].bit();
            i += 1;
        }
        Perms(bits)
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

    /// These, minus the ones in `other`.
    #[must_use]
    pub fn without(self, other: Perms) -> Perms {
        Perms(self.0 & !other.0)
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
            // Bits this build doesn't know are dropped.
            return u16::deserialize(d).map(|bits| Perms(bits) & Perms::ALL);
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
pub struct Rank(u32);

impl Rank {
    pub const EVERYONE: Rank = Rank(0);
    pub const OWNER: Rank = Rank(u32::MAX);
}

const EVERYONE_PERMS: Perms =
    Perms::of(&[Perm::View, Perm::Join, Perm::Speak, Perm::SendMessages, Perm::CreateTemporary]);
const MODERATOR_PERMS: Perms =
    EVERYONE_PERMS.union(Perms::of(&[Perm::Bypass, Perm::MoveMembers, Perm::MuteMembers, Perm::Kick]));
const ADMIN_PERMS: Perms =
    MODERATOR_PERMS.union(Perms::of(&[Perm::ManageChannels, Perm::ManagePermissions, Perm::Ban, Perm::ManageRoles]));
/// On top of the Moderator role, for the owner of a temporary channel in it:
/// rename it, change its limit and password, and delete it.
const ROOM_OWNER_PERMS: Perms = Perms::of(&[Perm::ManageChannels]);

/// Every role on a server and who holds them.
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

/// Who is asking: the person, and the connection, which may own a temporary channel.
#[derive(Debug, Clone, Copy)]
pub struct Subject {
    pub client: ClientId,
    pub identity: Identity,
}

/// What someone may do in one place, and how high they stand there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    pub perms: Perms,
    pub rank: Rank,
}

/// A channel's explicit setting for one role: each permission allowed,
/// denied, or (in neither) inherited from the channel above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Override {
    allow: Perms,
    deny: Perms,
}

impl Override {
    /// A permission in both is allowed.
    pub fn new(allow: Perms, deny: Perms) -> Self {
        Self { allow, deny: deny.without(allow) }
    }
}

/// The channels resolution looks at.
pub trait ChannelTree {
    fn channel(&self, id: ChannelId) -> Option<&ChannelInfo>;
    /// The channel's explicit settings, per role.
    fn overrides(&self, id: ChannelId) -> &[(RoleId, Override)];
}

impl Roles {
    pub fn role(&self, id: RoleId) -> Option<&Role> {
        self.ranked.iter().find(|r| r.id == id)
    }

    pub fn height(&self, role: RoleId) -> Rank {
        let below = self.ranked.iter().position(|r| r.id == role).map_or(0, |i| self.ranked.len() - i);
        Rank(below as u32)
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

    /// What `who` may do in `channel`, or on the server as a whole with `None`.
    ///
    /// The owner may do everything. Anyone else has what their roles grant,
    /// Everyone's included. In a channel, and every channel below it, its
    /// moderators and the owner of a temporary channel also hold the Moderator
    /// role. Then each permission a channel sets for one of their roles
    /// overrides what the roles grant; the closest channel that sets it wins,
    /// and there an allow beats a deny.
    pub fn resolve(&self, who: Subject, channel: Option<ChannelId>, tree: &impl ChannelTree) -> Standing {
        if self.owner == Some(who.identity) {
            return Standing { perms: Perms::ALL, rank: Rank::OWNER };
        }
        let mut path = Vec::new();
        let mut at = channel.and_then(|id| tree.channel(id));
        while let Some(c) = at {
            path.push(c);
            at = c.parent.and_then(|p| tree.channel(p));
        }
        let owns_room = path.iter().any(|c| c.temporary && c.creator == Some(who.client));
        let moderates = path.iter().any(|c| c.moderators.contains(&who.identity));

        let mut held = vec![RoleId::EVERYONE];
        held.extend_from_slice(self.assigned(who.identity));
        if owns_room || moderates {
            held.push(RoleId::MODERATOR);
        }
        let mut perms = held.iter().fold(Perms::NONE, |p, r| p | self.base(*r));
        let mut settled = Perms::NONE;
        for c in &path {
            let (mut allow, mut deny) = (Perms::NONE, Perms::NONE);
            for (role, o) in tree.overrides(c.id) {
                if held.contains(role) {
                    allow |= o.allow;
                    deny |= o.deny;
                }
            }
            let (allow, deny) = (allow.without(settled), deny.without(allow).without(settled));
            perms = (perms | allow).without(deny);
            settled = settled | allow | deny;
        }
        if owns_room {
            perms |= ROOM_OWNER_PERMS;
        }
        let rank = held.iter().map(|r| self.height(*r)).max().unwrap_or(Rank::EVERYONE);
        Standing { perms, rank }
    }
}

/// What one client may do: on the server as a whole, and in each channel.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Permissions {
    pub server: Perms,
    pub channels: Vec<(ChannelId, Perms)>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

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

    #[derive(Default)]
    struct Tree {
        channels: HashMap<ChannelId, ChannelInfo>,
        overrides: HashMap<ChannelId, Vec<(RoleId, Override)>>,
    }

    impl ChannelTree for Tree {
        fn channel(&self, id: ChannelId) -> Option<&ChannelInfo> {
            self.channels.get(&id)
        }
        fn overrides(&self, id: ChannelId) -> &[(RoleId, Override)] {
            self.overrides.get(&id).map_or(&[], Vec::as_slice)
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
            creator: None,
            moderators: moderators.to_vec(),
        };
        let raid = ChannelInfo { temporary: true, creator: Some(9), ..ch(RAID, None, &[]) };
        let channels = [
            ch(LOBBY, None, &[]),
            ch(GAMING, None, &[M]),
            ch(SQUAD, Some(GAMING), &[]),
            ch(DEEP, Some(SQUAD), &[]),
            ch(AFK, None, &[]),
            raid,
        ];
        Tree { channels: channels.into_iter().map(|c| (c.id, c)).collect(), overrides: HashMap::new() }
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

    fn perms(roles: &Roles, tree: &Tree, identity: Identity, channel: Option<ChannelId>) -> Perms {
        roles.resolve(who(identity), channel, tree).perms
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
    }

    #[test]
    fn a_fresh_server_has_the_default_roles() {
        let (roles, t) = (roles(), tree());
        let everyone = Perms::of(&[Perm::View, Perm::Join, Perm::Speak, Perm::SendMessages, Perm::CreateTemporary]);
        assert_eq!(perms(&roles, &t, NOBODY, None), everyone);
        let moderator = everyone | Perms::of(&[Perm::Bypass, Perm::MoveMembers, Perm::MuteMembers, Perm::Kick]);
        assert_eq!(perms(&roles, &t, MODERATOR, None), moderator);
        assert_eq!(perms(&roles, &t, ADMIN, None), Perms::ALL);
        assert_eq!(perms(&roles, &t, OWNER, Some(AFK)), Perms::ALL);
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

        // Holding both, the higher one counts; reordering changes which that is.
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
            let standing = roles.resolve(who(M), Some(channel), &t);
            assert_eq!(standing, Standing { perms: moderator, rank: roles.height(RoleId::MODERATOR) }, "{channel}");
        }
        for channel in [None, Some(LOBBY), Some(AFK)] {
            assert_eq!(roles.resolve(who(M), channel, &t).rank, Rank::EVERYONE, "{channel:?}");
            assert!(!perms(&roles, &t, M, channel).has(Perm::Kick), "{channel:?}");
        }
        assert_eq!(roles.resolve(who(ADMIN), Some(GAMING), &t).rank, roles.rank(ADMIN), "an admin stays an admin");
    }

    #[test]
    fn room_owners_moderate_and_manage_their_room() {
        let (roles, t) = (roles(), tree());
        let creator = Subject { client: 9, identity: NOBODY };
        let room = roles.resolve(creator, Some(RAID), &t);
        assert!(room.perms.has(Perm::Kick) && room.perms.has(Perm::ManageChannels));
        assert!(!room.perms.has(Perm::Ban));
        assert_eq!(room.rank, roles.height(RoleId::MODERATOR));
        assert!(!roles.resolve(creator, Some(LOBBY), &t).perms.has(Perm::Kick), "elsewhere");
        let other = Subject { client: 8, identity: NOBODY };
        assert!(!roles.resolve(other, Some(RAID), &t).perms.has(Perm::Kick), "another connection");
    }

    #[test]
    fn the_closest_channel_setting_wins_and_there_allow_beats_deny() {
        let (roles, mut t) = (roles(), tree());
        let (view, speak) = (Perms::of(&[Perm::View]), Perms::of(&[Perm::Speak]));
        // Gaming is hidden and silent for everyone, Squad lets them speak, and
        // moderators see Deep.
        t.overrides.insert(GAMING, vec![(RoleId::EVERYONE, Override::new(Perms::NONE, view | speak))]);
        t.overrides.insert(SQUAD, vec![(RoleId::EVERYONE, Override::new(speak, Perms::NONE))]);
        t.overrides.insert(DEEP, vec![(RoleId::MODERATOR, Override::new(view, Perms::NONE))]);

        assert!(!perms(&roles, &t, NOBODY, Some(GAMING)).has(Perm::View));
        assert!(!perms(&roles, &t, NOBODY, Some(SQUAD)).has(Perm::View), "inherited from Gaming");
        assert!(perms(&roles, &t, NOBODY, Some(SQUAD)).has(Perm::Speak), "Squad's own setting is closer");
        assert!(perms(&roles, &t, NOBODY, Some(LOBBY)).has(Perm::View), "elsewhere");
        assert!(perms(&roles, &t, MODERATOR, Some(DEEP)).has(Perm::View), "the moderators' allow is closer");
        assert!(!perms(&roles, &t, NOBODY, Some(DEEP)).has(Perm::View));

        // At one level, one role's allow beats another's deny.
        t.overrides.insert(
            GAMING,
            vec![
                (RoleId::EVERYONE, Override::new(Perms::NONE, view)),
                (RoleId::MODERATOR, Override::new(view, Perms::NONE)),
            ],
        );
        assert!(perms(&roles, &t, MODERATOR, Some(GAMING)).has(Perm::View));
        assert!(perms(&roles, &t, M, Some(SQUAD)).has(Perm::View), "listed moderators hold the role there");
        assert!(!perms(&roles, &t, NOBODY, Some(GAMING)).has(Perm::View));
        assert_eq!(perms(&roles, &t, OWNER, Some(GAMING)), Perms::ALL, "nothing overrides the owner");
        assert_eq!(perms(&roles, &t, NOBODY, None), roles.everyone, "nor applies to the server as a whole");
    }
}
