//! A client-side mirror of the server state, kept in sync by applying
//! [`S2C`] events.

use std::collections::HashMap;

use crate::perms::{self, Action};
use crate::{ChannelId, ChannelInfo, ClientId, ClientInfo, S2C, ServerInfo};

#[derive(Debug, Clone)]
pub struct ServerView {
    pub my_id: ClientId,
    pub info: ServerInfo,
    pub channels: HashMap<ChannelId, ChannelInfo>,
    pub clients: HashMap<ClientId, ClientInfo>,
    /// Channel tree in display order: (depth, channel id).
    tree: Vec<(usize, ChannelId)>,
}

impl ServerView {
    pub fn new(my_id: ClientId, info: ServerInfo, channels: Vec<ChannelInfo>, clients: Vec<ClientInfo>) -> Self {
        let mut view = Self {
            my_id,
            info,
            channels: channels.into_iter().map(|c| (c.id, c)).collect(),
            clients: clients.into_iter().map(|c| (c.id, c)).collect(),
            tree: Vec::new(),
        };
        view.rebuild_tree();
        view
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
            S2C::ClientAdmin { id, admin } => {
                if let Some(c) = self.clients.get_mut(id) {
                    c.admin = *admin;
                }
            }
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

    /// Whether the rules let this client do it: what the UI offers.
    pub fn allowed(&self, action: Action<'_>) -> bool {
        self.me().is_some_and(|me| perms::allowed(me, action, &self.channels))
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
            admin: false,
            identity: crate::Identity([1; 32]),
        };
        let mut v = ServerView::new(1, info, vec![chan(3, None, 1), chan(1, None, 0), chan(2, Some(1), 0)], vec![me]);
        assert_eq!(v.tree(), &[(0, 1), (1, 2), (0, 3)]);

        v.apply(&S2C::Channels(vec![chan(1, None, 1), chan(3, None, 0)]));
        assert_eq!(v.tree(), &[(0, 3), (0, 1)], "channel list replaced and re-sorted");

        v.apply(&S2C::ClientMoved { id: 1, channel: 3 });
        assert_eq!(v.my_channel(), Some(3));
        v.apply(&S2C::ClientLeft { id: 1, reason: LeaveReason::Disconnected });
        assert!(v.me().is_none());
    }
}
