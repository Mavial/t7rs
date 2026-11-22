//! A client-side mirror of the server state, kept in sync by applying
//! [`S2C`] events. Lives in the protocol crate so the semantics of each
//! event are defined in exactly one place.

use std::collections::HashMap;

use crate::perms::{Perm, Permissions, Perms, Roles};
use crate::{ChannelId, ChannelInfo, ClientId, ClientInfo, Identity, S2C, ServerInfo};

#[derive(Debug, Clone)]
pub struct ServerView {
    pub my_id: ClientId,
    pub info: ServerInfo,
    pub channels: HashMap<ChannelId, ChannelInfo>,
    pub clients: HashMap<ClientId, ClientInfo>,
    /// Every role and who holds them, while this client may manage roles.
    pub roles: Option<Roles>,
    server_perms: Perms,
    channel_perms: HashMap<ChannelId, Perms>,
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
            server_perms: Perms::NONE,
            channel_perms: HashMap::new(),
            tree: Vec::new(),
        };
        view.set_permissions(permissions);
        view.rebuild_tree();
        view
    }

    fn set_permissions(&mut self, permissions: Permissions) {
        self.server_perms = permissions.server;
        self.channel_perms = permissions.channels.into_iter().collect();
        if !self.server_perms.has(Perm::ManageRoles) {
            self.roles = None;
        }
    }

    /// Applies an event. Returns `false` for events that don't touch state.
    pub fn apply(&mut self, event: &S2C) -> bool {
        match event {
            S2C::ClientJoined(info) => {
                self.clients.insert(info.id, info.clone());
            }
            S2C::ClientLeft { id, .. } => {
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
        self.server_perms.has(perm)
    }

    /// Whether the server lets this client do it in `channel`.
    pub fn may_in(&self, channel: ChannelId, perm: Perm) -> bool {
        self.channel_perms.get(&channel).is_some_and(|p| p.has(perm))
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

#[cfg(test)]
mod tests {
    use super::*;
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
            creator: None,
            moderators: Vec::new(),
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
        };
        let permissions = Permissions { server: Perms::NONE, channels: vec![(2, Perms::of(&[Perm::Kick]))] };
        let channels = vec![chan(3, None, 1), chan(1, None, 0), chan(2, Some(1), 0)];
        let mut v = ServerView::new(1, info, channels, vec![me], permissions);
        assert_eq!(v.tree(), &[(0, 1), (1, 2), (0, 3)]);
        assert!(v.may_in(2, Perm::Kick) && !v.may_in(1, Perm::Kick) && !v.may(Perm::Kick));

        v.apply(&S2C::Channels(vec![chan(1, None, 1), chan(3, None, 0)]));
        assert_eq!(v.tree(), &[(0, 3), (0, 1)], "channel list replaced and re-sorted");

        v.apply(&S2C::Permissions(Permissions { server: Perms::ALL, channels: Vec::new() }));
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
