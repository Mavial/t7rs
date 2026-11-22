//! Who may do what. The server enforces these rules and the client asks the
//! same questions to decide which menu entries to show.
//!
//! Roles, strongest first: server admins (logged in with the admin token),
//! channel moderators, everyone. A moderator of a channel moderates it and
//! every channel below it.

use std::collections::HashMap;

use crate::{ChannelId, ChannelInfo, ClientInfo, Identity};

/// Something one person may or may not do.
#[derive(Debug, Clone, Copy)]
pub enum Action<'a> {
    /// Temporary channels are open to everyone.
    CreatePermanentChannel,
    EditChannel(ChannelId),
    DeleteChannel(ChannelId),
    /// Move channels and pick the default one.
    RearrangeChannels,
    /// Join a password-protected channel without its password.
    SkipChannelPassword,
    /// Out of their channel (to the default one), or off the server.
    Kick { target: &'a ClientInfo, from_server: bool },
    Ban(&'a ClientInfo),
    /// See the ban list and lift bans.
    ManageBans,
    SetModerator,
}

/// The channels the rules look at: the server's tree or a client's mirror of it.
pub trait ChannelTree {
    fn channel(&self, id: ChannelId) -> Option<&ChannelInfo>;
}

impl ChannelTree for HashMap<ChannelId, ChannelInfo> {
    fn channel(&self, id: ChannelId) -> Option<&ChannelInfo> {
        self.get(&id)
    }
}

pub fn allowed(actor: &ClientInfo, action: Action<'_>, tree: &impl ChannelTree) -> bool {
    match action {
        Action::Kick { target, .. } | Action::Ban(target) if same_person(actor, target) => false,
        // There is nowhere to kick them to.
        Action::Kick { target, from_server: false } if tree.channel(target.channel).is_none_or(|c| c.default) => false,
        _ if actor.admin => true,
        Action::EditChannel(id) => created(actor, id, tree) || moderated_from(actor.identity, id, tree).is_some(),
        Action::DeleteChannel(id) => created(actor, id, tree),
        Action::Kick { target, from_server: false } => {
            let Some(mine) = moderated_from(actor.identity, target.channel, tree) else { return false };
            let theirs = moderated_from(target.identity, target.channel, tree);
            // A moderator outranks only moderators of channels below their own.
            !target.admin && theirs.is_none_or(|theirs| theirs > mine)
        }
        _ => false,
    }
}

/// The depth (top level 1) of the highest channel at or above `channel` that
/// `identity` moderates, or `None` if they don't moderate it.
pub fn moderated_from(identity: Identity, channel: ChannelId, tree: &impl ChannelTree) -> Option<usize> {
    let mut path = Vec::new();
    let mut at = tree.channel(channel);
    while let Some(c) = at {
        path.push(c);
        at = c.parent.and_then(|p| tree.channel(p));
    }
    // `path` runs from `channel` up to the top level.
    path.iter().rev().position(|c| c.moderators.contains(&identity)).map(|i| i + 1)
}

/// The creator of a temporary channel keeps managing it.
fn created(actor: &ClientInfo, channel: ChannelId, tree: &impl ChannelTree) -> bool {
    tree.channel(channel).is_some_and(|c| c.temporary && c.creator == Some(actor.id))
}

/// One person may be connected twice with the same identity.
fn same_person(a: &ClientInfo, b: &ClientInfo) -> bool {
    a.id == b.id || a.identity == b.identity
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientStatus;

    // Lobby (default)
    // Gaming            moderated by G and G2
    //   Squad           moderated by S
    //     Deep
    // AFK               moderated by A
    // Raid (temporary, made by client 9)
    const LOBBY: ChannelId = 1;
    const GAMING: ChannelId = 2;
    const SQUAD: ChannelId = 3;
    const DEEP: ChannelId = 4;
    const AFK: ChannelId = 5;
    const RAID: ChannelId = 6;
    const G: Identity = Identity([0xa; 32]);
    const G2: Identity = Identity([0xd; 32]);
    const S: Identity = Identity([0xb; 32]);
    const A: Identity = Identity([0xc; 32]);

    fn tree() -> HashMap<ChannelId, ChannelInfo> {
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
        [ch(LOBBY, None, &[]), ch(GAMING, None, &[G, G2]), ch(SQUAD, Some(GAMING), &[S]), ch(DEEP, Some(SQUAD), &[]), ch(AFK, None, &[A]), raid]
            .into_iter()
            .map(|c| (c.id, c))
            .collect()
    }

    fn client(id: u32, identity: Identity, channel: ChannelId) -> ClientInfo {
        ClientInfo { id, nickname: format!("u{id}"), channel, status: ClientStatus::default(), admin: false, identity }
    }

    fn admin(id: u32, channel: ChannelId) -> ClientInfo {
        ClientInfo { admin: true, ..client(id, Identity([id as u8; 32]), channel) }
    }

    fn user(id: u32, channel: ChannelId) -> ClientInfo {
        client(id, Identity([id as u8; 32]), channel)
    }

    fn can(actor: &ClientInfo, action: Action<'_>) -> bool {
        allowed(actor, action, &tree())
    }

    fn kick(target: &ClientInfo) -> Action<'_> {
        Action::Kick { target, from_server: false }
    }

    #[test]
    fn moderation_reaches_down_the_tree() {
        let t = tree();
        assert_eq!(moderated_from(G, GAMING, &t), Some(1));
        assert_eq!(moderated_from(G, DEEP, &t), Some(1));
        assert_eq!(moderated_from(S, DEEP, &t), Some(2));
        assert_eq!(moderated_from(S, GAMING, &t), None, "not up the tree");
        assert_eq!(moderated_from(G, AFK, &t), None, "not sideways");
        assert_eq!(moderated_from(G, 99, &t), None);
    }

    #[test]
    fn moderators_edit_their_channels_and_nothing_else() {
        let g = client(1, G, LOBBY);
        for (channel, may) in [(GAMING, true), (SQUAD, true), (DEEP, true), (AFK, false), (LOBBY, false)] {
            assert_eq!(can(&g, Action::EditChannel(channel)), may, "edit channel {channel}");
            assert!(!can(&g, Action::DeleteChannel(channel)), "delete channel {channel}");
        }
        for action in [
            Action::CreatePermanentChannel,
            Action::RearrangeChannels,
            Action::SkipChannelPassword,
            Action::ManageBans,
            Action::SetModerator,
            Action::Ban(&user(2, GAMING)),
            Action::Kick { target: &user(2, GAMING), from_server: true },
        ] {
            assert!(!can(&g, action), "{action:?}");
        }
    }

    #[test]
    fn creators_keep_their_temporary_channels() {
        let creator = user(9, RAID);
        assert!(can(&creator, Action::EditChannel(RAID)));
        assert!(can(&creator, Action::DeleteChannel(RAID)));
        assert!(!can(&user(8, RAID), Action::EditChannel(RAID)));
        assert!(!can(&creator, kick(&user(8, RAID))), "making a channel doesn't make you its moderator");
    }

    #[test]
    fn who_a_moderator_may_kick() {
        let g = client(1, G, GAMING);
        let s = client(2, S, SQUAD);
        // Everyone in the moderated channels, from the top channel down.
        for channel in [GAMING, SQUAD, DEEP] {
            assert!(can(&g, kick(&user(3, channel))), "user in channel {channel}");
        }
        assert!(!can(&g, kick(&user(3, AFK))), "outside their channels");
        assert!(!can(&g, kick(&user(3, LOBBY))));
        // A moderator of a lower channel, but not of an equal or higher one.
        assert!(can(&g, kick(&s)));
        assert!(!can(&s, kick(&g)), "g moderates a higher channel");
        assert!(!can(&s, kick(&client(4, G, SQUAD))), "g moderates Squad from higher up");
        assert!(!can(&s, kick(&client(4, S, DEEP))), "same identity, other connection");
        assert!(!can(&g, kick(&client(5, G2, GAMING))), "another moderator of Gaming");
        // A moderator elsewhere is an ordinary user here.
        assert!(can(&g, kick(&client(4, A, GAMING))));
        assert!(!can(&g, kick(&admin(6, GAMING))), "admins");
        assert!(!can(&g, kick(&g)), "themselves");
        assert!(!can(&user(7, GAMING), kick(&user(3, GAMING))), "everyone else");
    }

    #[test]
    fn admins_may_everything_but_kick_or_ban_themselves() {
        let a = admin(1, LOBBY);
        let other_admin = admin(2, GAMING);
        let target = user(3, GAMING);
        for action in [
            Action::CreatePermanentChannel,
            Action::EditChannel(AFK),
            Action::DeleteChannel(AFK),
            Action::RearrangeChannels,
            Action::SkipChannelPassword,
            Action::ManageBans,
            Action::SetModerator,
            kick(&target),
            Action::Kick { target: &target, from_server: true },
            Action::Ban(&target),
            Action::Ban(&other_admin),
        ] {
            assert!(can(&a, action), "{action:?}");
        }
        let twin = ClientInfo { id: 4, ..a.clone() };
        for target in [&a, &twin] {
            assert!(!can(&a, Action::Kick { target, from_server: true }));
            assert!(!can(&a, Action::Ban(target)));
        }
    }

    #[test]
    fn channel_kicks_need_somewhere_to_send_people() {
        let mut t = tree();
        t.get_mut(&LOBBY).unwrap().moderators.push(G);
        let in_lobby = user(3, LOBBY);
        assert!(!allowed(&client(1, G, GAMING), kick(&in_lobby), &t), "moderator of the default channel");
        assert!(!allowed(&admin(2, GAMING), kick(&in_lobby), &t), "admin");
        assert!(allowed(&admin(2, GAMING), Action::Kick { target: &in_lobby, from_server: true }, &t));
    }
}
