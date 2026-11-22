//! The channel tree: structure, validation, ordering and the saved format.
//! Pure state, no networking: who may do what is decided by the caller.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use t7_proto::perms::ChannelTree;
use t7_proto::{
    ChannelId, ChannelInfo, ChannelSettings, ClientId, Identity, MAX_CHANNEL_DEPTH, MAX_CHANNEL_NAME_LEN,
    MAX_CHANNEL_TOPIC_LEN, MAX_CHANNELS, PasswordChange,
};

use crate::config::ChannelConfig;

/// Upper bound for a channel's user limit.
const MAX_LIMIT: u32 = 1000;

struct Channel {
    info: ChannelInfo,
    password: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    NotFound,
    BadName,
    NameTaken,
    TopicTooLong,
    BadLimit,
    TooDeep,
    TooMany,
    DefaultChannel,
    PermanentUnderTemporary,
    IntoItself,
}

impl ChannelError {
    pub fn message(self) -> String {
        match self {
            ChannelError::NotFound => "channel does not exist".into(),
            ChannelError::BadName => format!("channel names need 1 to {MAX_CHANNEL_NAME_LEN} characters"),
            ChannelError::NameTaken => "a channel with that name already exists there".into(),
            ChannelError::TopicTooLong => format!("topics can have at most {MAX_CHANNEL_TOPIC_LEN} characters"),
            ChannelError::BadLimit => format!("the user limit must be between 1 and {MAX_LIMIT}"),
            ChannelError::TooDeep => format!("channels can be nested at most {MAX_CHANNEL_DEPTH} levels deep"),
            ChannelError::TooMany => format!("the server has the maximum of {MAX_CHANNELS} channels"),
            ChannelError::DefaultChannel => "the default channel can't be deleted or made temporary".into(),
            ChannelError::PermanentUnderTemporary => "permanent channels can't go inside a temporary one".into(),
            ChannelError::IntoItself => "a channel can't be moved inside itself".into(),
        }
    }
}

/// `channels.toml`: the same `[[channels]]` format as the server config.
#[derive(Serialize, Deserialize)]
struct ChannelsFile {
    channels: Vec<ChannelConfig>,
}

pub struct Channels {
    map: BTreeMap<ChannelId, Channel>,
    default: ChannelId,
    next_id: ChannelId,
}

impl Channels {
    /// Builds the tree from config entries. An empty list gets a single "Lobby".
    pub fn from_config(list: &[ChannelConfig]) -> Self {
        let mut channels = Self { map: BTreeMap::new(), default: 0, next_id: 1 };
        let mut default = None;
        channels.add_config(list, None, &mut default);
        if channels.map.is_empty() {
            let lobby = ChannelConfig { name: "Lobby".into(), ..Default::default() };
            channels.add_config(&[lobby], None, &mut default);
        }
        let default = default.unwrap_or_else(|| *channels.map.keys().next().expect("at least one"));
        channels.set_default(default).expect("a permanent channel");
        channels
    }

    fn add_config(&mut self, list: &[ChannelConfig], parent: Option<ChannelId>, default: &mut Option<ChannelId>) {
        for (order, c) in list.iter().enumerate() {
            let id = self.next_id;
            self.next_id += 1;
            let password = c.password.clone().filter(|p| !p.is_empty());
            let info = ChannelInfo {
                id,
                parent,
                name: c.name.clone(),
                topic: c.topic.clone(),
                max_clients: c.max_clients,
                order: order as u32,
                temporary: false,
                has_password: password.is_some(),
                default: false,
                creator: None,
                moderators: c.moderators.clone(),
            };
            self.map.insert(id, Channel { info, password });
            if c.default && default.is_none() {
                *default = Some(id);
            }
            self.add_config(&c.children, Some(id), default);
        }
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let file: ChannelsFile = toml::from_str(&text).map_err(std::io::Error::other)?;
        Ok(Self::from_config(&file.channels))
    }

    /// Writes the permanent channels; write-then-rename, so a crash never truncates the file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let header = "# The live channel tree, kept up to date by the server as admins edit\n\
                      # channels in T7. Edit it only while the server is stopped.\n\n";
        let body = toml::to_string(&ChannelsFile { channels: self.to_config() }).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, format!("{header}{body}"))?;
        std::fs::rename(&tmp, path)
    }

    /// Permanent channels in the saved format, siblings in display order.
    pub fn to_config(&self) -> Vec<ChannelConfig> {
        self.children_of(None).into_iter().filter_map(|id| self.config_of(id)).collect()
    }

    fn config_of(&self, id: ChannelId) -> Option<ChannelConfig> {
        let c = &self.map[&id];
        if c.info.temporary {
            return None;
        }
        Some(ChannelConfig {
            name: c.info.name.clone(),
            topic: c.info.topic.clone(),
            max_clients: c.info.max_clients,
            password: c.password.clone(),
            default: c.info.default,
            moderators: c.info.moderators.clone(),
            children: self.children_of(Some(id)).into_iter().filter_map(|k| self.config_of(k)).collect(),
        })
    }

    /// Everything clients need to draw the tree.
    pub fn snapshot(&self) -> Vec<ChannelInfo> {
        self.map.values().map(|c| c.info.clone()).collect()
    }

    pub fn default_id(&self) -> ChannelId {
        self.default
    }

    pub fn get(&self, id: ChannelId) -> Option<&ChannelInfo> {
        self.map.get(&id).map(|c| &c.info)
    }

    /// All channel ids, parents before children, siblings in order.
    pub fn ids_in_order(&self) -> Vec<ChannelId> {
        let mut out = Vec::with_capacity(self.map.len());
        let mut stack: Vec<ChannelId> = self.children_of(None).into_iter().rev().collect();
        while let Some(id) = stack.pop() {
            out.push(id);
            stack.extend(self.children_of(Some(id)).into_iter().rev());
        }
        out
    }

    pub fn password_ok(&self, id: ChannelId, given: Option<&str>) -> bool {
        match self.map.get(&id).and_then(|c| c.password.as_deref()) {
            Some(pw) => given == Some(pw),
            None => true,
        }
    }

    pub fn create(
        &mut self,
        parent: Option<ChannelId>,
        settings: &ChannelSettings,
        temporary: bool,
        creator: ClientId,
    ) -> Result<ChannelId, ChannelError> {
        if self.map.len() >= MAX_CHANNELS {
            return Err(ChannelError::TooMany);
        }
        if let Some(p) = parent {
            let parent = self.map.get(&p).ok_or(ChannelError::NotFound)?;
            if parent.info.temporary && !temporary {
                return Err(ChannelError::PermanentUnderTemporary);
            }
            if self.depth(p) + 1 > MAX_CHANNEL_DEPTH {
                return Err(ChannelError::TooDeep);
            }
        }
        let (name, topic, limit) = self.validate(settings, parent, None)?;
        let password = match &settings.password {
            PasswordChange::Set(p) if !p.is_empty() => Some(p.clone()),
            _ => None,
        };
        let id = self.next_id;
        self.next_id += 1;
        let info = ChannelInfo {
            id,
            parent,
            name,
            topic,
            max_clients: limit,
            order: self.children_of(parent).len() as u32,
            temporary,
            has_password: password.is_some(),
            default: false,
            creator: temporary.then_some(creator),
            moderators: Vec::new(),
        };
        self.map.insert(id, Channel { info, password });
        Ok(id)
    }

    pub fn edit(&mut self, id: ChannelId, settings: &ChannelSettings) -> Result<(), ChannelError> {
        let parent = self.map.get(&id).ok_or(ChannelError::NotFound)?.info.parent;
        let (name, topic, limit) = self.validate(settings, parent, Some(id))?;
        let c = self.map.get_mut(&id).expect("checked");
        c.info.name = name;
        c.info.topic = topic;
        c.info.max_clients = limit;
        match &settings.password {
            PasswordChange::Keep => {}
            PasswordChange::Remove => c.password = None,
            PasswordChange::Set(p) => c.password = Some(p.clone()).filter(|p| !p.is_empty()),
        }
        c.info.has_password = c.password.is_some();
        Ok(())
    }

    /// Moves `id` under `parent`, at `position` among its new siblings.
    pub fn move_to(&mut self, id: ChannelId, parent: Option<ChannelId>, position: u32) -> Result<(), ChannelError> {
        let moving = self.map.get(&id).ok_or(ChannelError::NotFound)?;
        let temporary = moving.info.temporary;
        let old_parent = moving.info.parent;
        if let Some(p) = parent {
            let target = self.map.get(&p).ok_or(ChannelError::NotFound)?;
            if self.subtree(id).contains(&p) {
                return Err(ChannelError::IntoItself);
            }
            if target.info.temporary && !temporary {
                return Err(ChannelError::PermanentUnderTemporary);
            }
            if self.depth(p) + self.height(id) > MAX_CHANNEL_DEPTH {
                return Err(ChannelError::TooDeep);
            }
        }
        if parent != old_parent {
            let name = self.map[&id].info.name.clone();
            if self.name_taken(&name, parent, Some(id)) {
                return Err(ChannelError::NameTaken);
            }
        }
        let mut siblings: Vec<ChannelId> = self.children_of(parent).into_iter().filter(|&s| s != id).collect();
        let at = (position as usize).min(siblings.len());
        siblings.insert(at, id);
        self.map.get_mut(&id).expect("checked").info.parent = parent;
        self.renumber(&siblings);
        if parent != old_parent {
            let old_siblings = self.children_of(old_parent);
            self.renumber(&old_siblings);
        }
        Ok(())
    }

    /// Removes `id` and everything below it. Returns the removed ids.
    pub fn delete(&mut self, id: ChannelId) -> Result<Vec<ChannelId>, ChannelError> {
        let parent = self.map.get(&id).ok_or(ChannelError::NotFound)?.info.parent;
        let removed = self.subtree(id);
        if removed.contains(&self.default) {
            return Err(ChannelError::DefaultChannel);
        }
        for r in &removed {
            self.map.remove(r);
        }
        let siblings = self.children_of(parent);
        self.renumber(&siblings);
        Ok(removed)
    }

    pub fn set_default(&mut self, id: ChannelId) -> Result<(), ChannelError> {
        let c = self.map.get(&id).ok_or(ChannelError::NotFound)?;
        if c.info.temporary {
            return Err(ChannelError::DefaultChannel);
        }
        if let Some(old) = self.map.get_mut(&self.default) {
            old.info.default = false;
        }
        self.map.get_mut(&id).expect("checked").info.default = true;
        self.default = id;
        Ok(())
    }

    /// Adds or removes a moderator. Returns whether anything changed.
    pub fn set_moderator(&mut self, id: ChannelId, identity: Identity, moderator: bool) -> Result<bool, ChannelError> {
        let list = &mut self.map.get_mut(&id).ok_or(ChannelError::NotFound)?.info.moderators;
        let had = list.contains(&identity);
        match (had, moderator) {
            (false, true) => list.push(identity),
            (true, false) => list.retain(|m| *m != identity),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `id` and all channels below it.
    pub fn subtree(&self, id: ChannelId) -> Vec<ChannelId> {
        let mut out = vec![id];
        let mut i = 0;
        while i < out.len() {
            let parent = out[i];
            out.extend(self.map.values().filter(|c| c.info.parent == Some(parent)).map(|c| c.info.id));
            i += 1;
        }
        out
    }

    /// Top-most temporary channels with nobody in them or below them.
    pub fn abandoned_temporaries(&self, occupied: &HashSet<ChannelId>) -> Vec<ChannelId> {
        self.map
            .values()
            .filter(|c| c.info.temporary)
            .filter(|c| !c.info.parent.is_some_and(|p| self.map.get(&p).is_some_and(|p| p.info.temporary)))
            .filter(|c| self.subtree(c.info.id).iter().all(|id| !occupied.contains(id)))
            .map(|c| c.info.id)
            .collect()
    }

    fn children_of(&self, parent: Option<ChannelId>) -> Vec<ChannelId> {
        let mut kids: Vec<&ChannelInfo> = self.map.values().map(|c| &c.info).filter(|c| c.parent == parent).collect();
        kids.sort_by_key(|c| (c.order, c.id));
        kids.into_iter().map(|c| c.id).collect()
    }

    fn renumber(&mut self, ordered: &[ChannelId]) {
        for (i, id) in ordered.iter().enumerate() {
            if let Some(c) = self.map.get_mut(id) {
                c.info.order = i as u32;
            }
        }
    }

    /// Top level is depth 1.
    fn depth(&self, id: ChannelId) -> usize {
        let mut depth = 1;
        let mut cur = self.map.get(&id).and_then(|c| c.info.parent);
        while let Some(p) = cur {
            depth += 1;
            cur = self.map.get(&p).and_then(|c| c.info.parent);
        }
        depth
    }

    /// Levels in the subtree under and including `id`.
    fn height(&self, id: ChannelId) -> usize {
        1 + self.children_of(Some(id)).into_iter().map(|k| self.height(k)).max().unwrap_or(0)
    }

    fn name_taken(&self, name: &str, parent: Option<ChannelId>, except: Option<ChannelId>) -> bool {
        self.map
            .values()
            .any(|c| c.info.parent == parent && Some(c.info.id) != except && c.info.name.eq_ignore_ascii_case(name))
    }

    fn validate(
        &self,
        s: &ChannelSettings,
        parent: Option<ChannelId>,
        except: Option<ChannelId>,
    ) -> Result<(String, String, Option<u32>), ChannelError> {
        let name: String = s.name.trim().chars().filter(|c| !c.is_control()).collect();
        if name.is_empty() || name.chars().count() > MAX_CHANNEL_NAME_LEN {
            return Err(ChannelError::BadName);
        }
        if self.name_taken(&name, parent, except) {
            return Err(ChannelError::NameTaken);
        }
        let topic = s.topic.trim().to_owned();
        if topic.chars().count() > MAX_CHANNEL_TOPIC_LEN {
            return Err(ChannelError::TopicTooLong);
        }
        if s.max_clients.is_some_and(|m| m == 0 || m > MAX_LIMIT) {
            return Err(ChannelError::BadLimit);
        }
        Ok((name, topic, s.max_clients))
    }
}

impl ChannelTree for Channels {
    fn channel(&self, id: ChannelId) -> Option<&ChannelInfo> {
        self.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(name: &str) -> ChannelSettings {
        ChannelSettings { name: name.into(), topic: String::new(), max_clients: None, password: PasswordChange::Keep }
    }

    fn cfg(name: &str, children: Vec<ChannelConfig>) -> ChannelConfig {
        ChannelConfig { name: name.into(), children, ..Default::default() }
    }

    fn names(ch: &Channels) -> Vec<String> {
        ch.ids_in_order().iter().map(|id| ch.get(*id).unwrap().name.clone()).collect()
    }

    fn three() -> Channels {
        Channels::from_config(&[cfg("Lobby", vec![]), cfg("Gaming", vec![cfg("Squad", vec![])]), cfg("AFK", vec![])])
    }

    #[test]
    fn builds_from_config_and_round_trips() {
        let mut list = vec![cfg("Lobby", vec![]), cfg("Gaming", vec![cfg("Squad", vec![])])];
        list[1].default = true;
        list[0].password = Some("pw".into());
        let ch = Channels::from_config(&list);
        assert_eq!(names(&ch), ["Lobby", "Gaming", "Squad"]);
        assert_eq!(ch.get(ch.default_id()).unwrap().name, "Gaming");
        assert!(ch.get(1).unwrap().has_password);
        assert_eq!(ch.to_config(), list);
        assert_eq!(names(&Channels::from_config(&[])), ["Lobby"]);
    }

    #[test]
    fn moderators_are_kept_with_their_channel() {
        let mut ch = three();
        let (a, b) = (Identity([1; 32]), Identity([2; 32]));
        assert_eq!(ch.set_moderator(3, a, true), Ok(true));
        assert_eq!(ch.set_moderator(3, a, true), Ok(false), "already a moderator");
        ch.set_moderator(3, b, true).unwrap();
        assert_eq!(ch.set_moderator(99, a, true), Err(ChannelError::NotFound));
        let saved = ch.to_config();
        assert_eq!(saved[1].children[0].moderators, [a, b]);
        assert_eq!(saved[0].moderators, [] as [Identity; 0]);
        assert!(!toml::to_string(&ChannelsFile { channels: vec![saved[0].clone()] }).unwrap().contains("moderators"));
        let text = toml::to_string(&ChannelsFile { channels: saved }).unwrap();
        let back = Channels::from_config(&toml::from_str::<ChannelsFile>(&text).unwrap().channels);
        assert_eq!(back.get(3).unwrap().moderators, [a, b]);

        assert_eq!(ch.set_moderator(3, a, false), Ok(true));
        assert_eq!(ch.get(3).unwrap().moderators, [b]);
    }

    #[test]
    fn the_default_channel_is_flagged() {
        let mut ch = three();
        let flagged = |ch: &Channels| ch.snapshot().iter().filter(|c| c.default).map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(flagged(&ch), [1]);
        assert!(ch.get(1).unwrap().default);
        ch.set_default(4).unwrap();
        assert_eq!(flagged(&ch), [4]);
        assert!(!ch.get(1).unwrap().default);
    }

    #[test]
    fn create_validates_and_orders() {
        let mut ch = three();
        let id = ch.create(None, &settings("  Music "), false, 1).unwrap();
        assert_eq!(names(&ch), ["Lobby", "Gaming", "Squad", "AFK", "Music"]);
        assert_eq!(ch.get(id).unwrap().name, "Music");
        assert_eq!(ch.create(None, &settings("music"), false, 1), Err(ChannelError::NameTaken));
        assert_eq!(ch.create(None, &settings(""), false, 1), Err(ChannelError::BadName));
        assert_eq!(ch.create(Some(99), &settings("x"), false, 1), Err(ChannelError::NotFound));
        let limit0 = ChannelSettings { max_clients: Some(0), ..settings("x") };
        assert_eq!(ch.create(None, &limit0, false, 1), Err(ChannelError::BadLimit));
        // Squad is depth 2; a child is depth 3 (allowed), a grandchild would be 4.
        let deep = ch.create(Some(3), &settings("Deep"), false, 1).unwrap();
        assert_eq!(ch.create(Some(deep), &settings("Deeper"), false, 1), Err(ChannelError::TooDeep));
    }

    #[test]
    fn temporary_channels_stay_temporary_and_vanish_when_empty() {
        let mut ch = three();
        let temp = ch.create(None, &settings("Raid"), true, 7).unwrap();
        assert_eq!(ch.get(temp).unwrap().creator, Some(7));
        assert_eq!(ch.create(Some(temp), &settings("Perm"), false, 7), Err(ChannelError::PermanentUnderTemporary));
        let sub = ch.create(Some(temp), &settings("Sub"), true, 7).unwrap();
        assert!(ch.to_config().iter().all(|c| c.name != "Raid"), "temporary channels aren't saved");

        // Someone in the sub-channel keeps the whole temporary tree alive.
        assert_eq!(ch.abandoned_temporaries(&HashSet::from([sub])), [] as [u32; 0]);
        assert_eq!(
            ch.abandoned_temporaries(&HashSet::from([1])),
            vec![temp],
            "only the top temporary channel is reported"
        );
        assert_eq!(ch.delete(temp).unwrap(), vec![temp, sub]);
        assert_eq!(ch.set_default(99), Err(ChannelError::NotFound));
    }

    #[test]
    fn move_reorders_and_refuses_nonsense() {
        let mut ch = three();
        ch.move_to(4, None, 0).unwrap(); // AFK to the top
        assert_eq!(names(&ch), ["AFK", "Lobby", "Gaming", "Squad"]);
        ch.move_to(3, None, 99).unwrap(); // Squad out of Gaming, to the end
        assert_eq!(names(&ch), ["AFK", "Lobby", "Gaming", "Squad"]);
        assert_eq!(ch.get(3).unwrap().parent, None);
        ch.move_to(3, Some(2), 0).unwrap();
        assert_eq!(ch.move_to(2, Some(3), 0), Err(ChannelError::IntoItself));
        let saved = ch.to_config();
        assert_eq!(saved.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["AFK", "Lobby", "Gaming"]);
        assert_eq!(saved[2].children[0].name, "Squad");
    }

    #[test]
    fn delete_protects_the_default_and_edits_apply() {
        let mut ch = three();
        assert_eq!(ch.delete(1), Err(ChannelError::DefaultChannel));
        ch.set_default(4).unwrap();
        assert_eq!(ch.delete(2).unwrap(), vec![2, 3]);
        assert_eq!(ch.delete(1).unwrap(), vec![1]);
        assert_eq!(names(&ch), ["AFK"]);

        let edit = ChannelSettings {
            topic: "zzz".into(),
            max_clients: Some(4),
            password: PasswordChange::Set("s".into()),
            ..settings("Away")
        };
        ch.edit(4, &edit).unwrap();
        let info = ch.get(4).unwrap();
        assert_eq!(
            (info.name.as_str(), info.topic.as_str(), info.max_clients, info.has_password),
            ("Away", "zzz", Some(4), true)
        );
        assert!(!ch.password_ok(4, Some("nope")) && ch.password_ok(4, Some("s")));
        ch.edit(4, &ChannelSettings { password: PasswordChange::Remove, ..settings("Away") }).unwrap();
        assert!(ch.password_ok(4, None));
    }
}
