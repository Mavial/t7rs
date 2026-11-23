//! A client-side mirror of the server state, kept in sync by applying
//! [`S2C`] events. Lives in the protocol crate so the semantics of each
//! event are defined in exactly one place.

use std::collections::HashMap;

use crate::perms::{ChannelTree, Perm, Permissions, Rank, RoleId, Roles};
use crate::{ChannelId, ChannelInfo, ClientId, ClientInfo, Identity, S2C, ServerInfo};

#[derive(Debug, Clone)]
pub struct ServerView {
    pub my_id: ClientId,
    pub info: ServerInfo,
    pub channels: HashMap<ChannelId, ChannelInfo>,
    pub clients: HashMap<ClientId, ClientInfo>,
    /// Every role and who holds them, while this client may edit them or
    /// channel permissions.
    pub roles: Option<Roles>,
    permissions: Permissions,
    /// Channel tree in display order: (depth, channel id).
    tree: Vec<(usize, ChannelId)>,
}

impl ServerView {
    pub fn new(
        my_id: ClientId,
        info: ServerInfo,
        channels: Vec<ChannelInfo>,
        clients: Vec<ClientInfo>,
        permissions: Permissions,
    ) -> Self {
        let mut view = Self {
            my_id,
            info,
            channels: channels.into_iter().map(|c| (c.id, c)).collect(),
            clients: clients.into_iter().map(|c| (c.id, c)).collect(),
            roles: None,
            permissions: Permissions::default(),
            tree: Vec::new(),
        };
        view.set_permissions(permissions);
        view.rebuild_tree();
        view
    }

    fn set_permissions(&mut self, permissions: Permissions) {
        if !permissions.edits_permissions() {
            self.roles = None;
        }
        self.permissions = permissions;
    }

    /// Applies an event. Returns `false` for events that don't touch state.
    pub fn apply(&mut self, event: &S2C) -> bool {
        match event {
            S2C::ClientJoined(info) | S2C::ClientShown(info) => {
                self.clients.insert(info.id, info.clone());
            }
            S2C::ClientLeft { id, .. } | S2C::ClientHidden { id } => {
                self.clients.remove(id);
            }
            S2C::ClientMoved { id, channel } => {
                if let Some(c) = self.clients.get_mut(id) {
                    c.channel = *channel;
                }
            }
            S2C::ClientStatus { id, status } => {
                if let Some(c) = self.clients.get_mut(id) {
                    c.status = *status;
                }
            }
            S2C::ClientRole { id, role } => {
                if let Some(c) = self.clients.get_mut(id) {
                    c.role.clone_from(role);
                }
            }
            S2C::ClientSpeech { id, speech } => {
                if let Some(c) = self.clients.get_mut(id) {
                    c.speech = *speech;
                }
            }
            S2C::Permissions(permissions) => self.set_permissions(permissions.clone()),
            S2C::Roles(roles) => self.roles = Some(roles.clone()),
            S2C::Channels(list) => {
                self.channels = list.iter().map(|c| (c.id, c.clone())).collect();
                self.rebuild_tree();
            }
            _ => return false,
        }
        true
    }

    pub fn me(&self) -> Option<&ClientInfo> {
        self.clients.get(&self.my_id)
    }

    pub fn my_channel(&self) -> Option<ChannelId> {
        self.me().map(|c| c.channel)
    }

    /// Whether the server lets this client do it on the server as a whole:
    /// what the UI offers. The server checks again.
    pub fn may(&self, perm: Perm) -> bool {
        self.permissions.server.has(perm)
    }

    /// Whether the server lets this client do it in `channel`.
    pub fn may_in(&self, channel: ChannelId, perm: Perm) -> bool {
        self.permissions.may_in(channel, perm)
    }

    pub fn may_rename(&self, channel: ChannelId) -> bool {
        self.permissions.may_rename(channel)
    }

    pub fn is_owner(&self) -> bool {
        let owner = self.roles.as_ref().and_then(|r| r.owner);
        owner.is_some_and(|o| self.me().is_some_and(|me| me.identity == o))
    }

    /// Whether `identity` is on the moderator list of `channel` or of a channel above it.
    pub fn moderates(&self, identity: Identity, channel: ChannelId) -> bool {
        let mut at = self.channels.get(&channel);
        while let Some(c) = at {
            if c.moderators.contains(&identity) {
                return true;
            }
            at = c.parent.and_then(|p| self.channels.get(&p));
        }
        false
    }

    pub fn my_rank_in(&self, channel: ChannelId) -> Rank {
        let (Some(roles), Some(me)) = (&self.roles, self.me()) else { return Rank::EVERYONE };
        let rank = roles.rank(me.identity);
        if self.moderates(me.identity, channel) { rank.max(roles.height(RoleId::MODERATOR)) } else { rank }
    }

    pub fn tree(&self) -> &[(usize, ChannelId)] {
        &self.tree
    }

    /// Clients in a channel, sorted by nickname (case-insensitive).
    pub fn clients_in(&self, channel: ChannelId) -> Vec<&ClientInfo> {
        let mut v: Vec<_> = self.clients.values().filter(|c| c.channel == channel).collect();
        v.sort_by_cached_key(|c| (c.nickname.to_lowercase(), c.id));
        v
    }

    pub fn nickname(&self, id: ClientId) -> Option<&str> {
        self.clients.get(&id).map(|c| c.nickname.as_str())
    }

    /// Whether a channel is hidden where the one above it is not: from
    /// people without a role, as far as the roles this client has show, or
    /// from this client itself, which sees it only because it manages it.
    pub fn hidden(&self, channel: ChannelId) -> bool {
        let hidden = |id| {
            let everyone = self.roles.as_ref().is_some_and(|r| !r.everyone_in(id, self).has(Perm::View));
            everyone || !self.may_in(id, Perm::View)
        };
        let parent = self.channels.get(&channel).and_then(|c| c.parent);
        hidden(channel) && !parent.is_some_and(hidden)
    }

    fn rebuild_tree(&mut self) {
        let mut children: HashMap<Option<ChannelId>, Vec<&ChannelInfo>> = HashMap::new();
        for c in self.channels.values() {
            children.entry(c.parent).or_default().push(c);
        }
        for list in children.values_mut() {
            list.sort_by_key(|c| (c.order, c.id));
        }
        let mut out = Vec::with_capacity(self.channels.len());
        let mut stack: Vec<(usize, ChannelId)> =
            children.get(&None).map_or_default(|l| l.iter().rev().map(|c| (0, c.id)).collect());
        while let Some((depth, id)) = stack.pop() {
            out.push((depth, id));
            if let Some(kids) = children.get(&Some(id)) {
                stack.extend(kids.iter().rev().map(|c| (depth + 1, c.id)));
            }
        }
        self.tree = out;
    }
}

impl ChannelTree for ServerView {
    fn channel(&self, id: ChannelId) -> Option<&ChannelInfo> {
        self.channels.get(&id)
    }

    fn channels(&self) -> impl Iterator<Item = &ChannelInfo> {
        self.channels.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perms::{ChannelPerms, Perms};
    use crate::{ClientStatus, LeaveReason};

    fn chan(id: u32, parent: Option<u32>, order: u32) -> ChannelInfo {
        ChannelInfo {
            id,
            parent,
            name: format!("c{id}"),
            topic: String::new(),
            max_clients: None,
            order,
            temporary: false,
            has_password: false,
            default: false,
            owner: None,
            moderators: Vec::new(),
            kind: crate::ChannelKind::Voice,
            overrides: Vec::new(),
        }
    }

    #[test]
    fn tree_order_and_events() {
        let info = ServerInfo { name: "s".into(), welcome: String::new(), max_clients: 10 };
        let me = ClientInfo {
            id: 1,
            nickname: "me".into(),
            channel: 1,
            status: ClientStatus::default(),
            role: None,
            identity: Identity([1; 32]),
            speech: crate::Speech::Allowed,
        };
        let kick = ChannelPerms { perms: Perms::of(&[Perm::Kick]), rename: false };
        let channels = [1, 3].map(|c| (c, ChannelPerms::default())).into_iter().chain([(2, kick)]).collect();
        let permissions = Permissions { server: Perms::NONE, channels };
        let channels = vec![chan(3, None, 1), chan(1, None, 0), chan(2, Some(1), 0)];
        let mut v = ServerView::new(1, info, channels, vec![me], permissions);
        assert_eq!(v.tree(), &[(0, 1), (1, 2), (0, 3)]);
        assert!(v.may_in(2, Perm::Kick) && !v.may_in(1, Perm::Kick) && !v.may(Perm::Kick));

        v.apply(&S2C::Channels(vec![chan(1, None, 1), chan(3, None, 0)]));
        assert_eq!(v.tree(), &[(0, 3), (0, 1)], "channel list replaced and re-sorted");

        v.apply(&S2C::Permissions(Permissions { server: Perms::ALL, channels: std::collections::BTreeMap::new() }));
        v.apply(&S2C::Roles(Roles::default()));
        assert!(v.may(Perm::Kick) && !v.may_in(2, Perm::Kick) && v.roles.is_some());
        v.apply(&S2C::Permissions(Permissions::default()));
        assert!(v.roles.is_none(), "dropped when that ends");

        v.apply(&S2C::ClientMoved { id: 1, channel: 3 });
        assert_eq!(v.my_channel(), Some(3));
        v.apply(&S2C::ClientLeft { id: 1, reason: LeaveReason::Disconnected });
        assert!(v.me().is_none());
    }
}
