use std::collections::HashMap;

use t7_proto::{ChannelId, ChannelInfo, MAX_CHANNEL_DEPTH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Before,
    Into,
    After,
}

impl Zone {
    pub fn at(fraction: f32) -> Self {
        if fraction < 0.3 {
            Zone::Before
        } else if fraction > 0.7 {
            Zone::After
        } else {
            Zone::Into
        }
    }
}

/// The arguments of `C2S::MoveChannel`: `position` is the index among the new
/// siblings once the dragged channel has left its old place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTarget {
    pub parent: Option<ChannelId>,
    pub position: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    IntoItself,
    PermanentUnderTemporary,
    TooDeep,
    NameTaken,
}

impl Refusal {
    pub fn message(self) -> String {
        match self {
            Refusal::IntoItself => "A channel can't go inside itself".into(),
            Refusal::PermanentUnderTemporary => "Permanent channels can't go inside a temporary one".into(),
            Refusal::TooDeep => format!("Channels nest at most {MAX_CHANNEL_DEPTH} levels deep"),
            Refusal::NameTaken => "A channel with that name is already there".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Move(DropTarget),
    Nothing,
    NotAllowed(Refusal),
}

pub fn drop_target(
    channels: &HashMap<ChannelId, ChannelInfo>,
    dragged: ChannelId,
    hovered: ChannelId,
    zone: Zone,
) -> Outcome {
    let (Some(moving), Some(row)) = (channels.get(&dragged), channels.get(&hovered)) else {
        return Outcome::Nothing;
    };
    if dragged == hovered {
        return Outcome::Nothing;
    }
    let parent = match zone {
        Zone::Into => Some(hovered),
        Zone::Before | Zone::After => row.parent,
    };
    let siblings: Vec<ChannelId> = children_of(channels, parent).into_iter().filter(|&s| s != dragged).collect();
    let position = match zone {
        Zone::Into => siblings.len(),
        Zone::Before => siblings.iter().position(|&s| s == hovered).unwrap_or(0),
        Zone::After => siblings.iter().position(|&s| s == hovered).map_or(siblings.len(), |i| i + 1),
    };

    if let Some(p) = parent {
        if is_within(channels, p, dragged) {
            return Outcome::NotAllowed(Refusal::IntoItself);
        }
        if channels[&p].temporary && !moving.temporary {
            return Outcome::NotAllowed(Refusal::PermanentUnderTemporary);
        }
        if depth(channels, p) + height(channels, dragged) > MAX_CHANNEL_DEPTH {
            return Outcome::NotAllowed(Refusal::TooDeep);
        }
    }
    if parent == moving.parent {
        let current = siblings_now(channels, dragged);
        if current == Some(position) {
            return Outcome::Nothing;
        }
    } else {
        let taken = channels
            .values()
            .any(|c| c.parent == parent && c.id != dragged && c.name.eq_ignore_ascii_case(&moving.name));
        if taken {
            return Outcome::NotAllowed(Refusal::NameTaken);
        }
    }
    Outcome::Move(DropTarget { parent, position: position as u32 })
}

fn children_of(channels: &HashMap<ChannelId, ChannelInfo>, parent: Option<ChannelId>) -> Vec<ChannelId> {
    let mut kids: Vec<&ChannelInfo> = channels.values().filter(|c| c.parent == parent).collect();
    kids.sort_by_key(|c| (c.order, c.id));
    kids.into_iter().map(|c| c.id).collect()
}

fn siblings_now(channels: &HashMap<ChannelId, ChannelInfo>, id: ChannelId) -> Option<usize> {
    children_of(channels, channels[&id].parent).iter().position(|&s| s == id)
}

fn is_within(channels: &HashMap<ChannelId, ChannelInfo>, id: ChannelId, ancestor: ChannelId) -> bool {
    let mut cur = Some(id);
    while let Some(c) = cur {
        if c == ancestor {
            return true;
        }
        cur = channels.get(&c).and_then(|c| c.parent);
    }
    false
}

fn depth(channels: &HashMap<ChannelId, ChannelInfo>, id: ChannelId) -> usize {
    let mut depth = 1;
    let mut cur = channels.get(&id).and_then(|c| c.parent);
    while let Some(p) = cur {
        depth += 1;
        cur = channels.get(&p).and_then(|c| c.parent);
    }
    depth
}

fn height(channels: &HashMap<ChannelId, ChannelInfo>, id: ChannelId) -> usize {
    1 + children_of(channels, Some(id)).into_iter().map(|k| height(channels, k)).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: ChannelId, parent: Option<ChannelId>, order: u32, name: &str) -> ChannelInfo {
        ChannelInfo {
            id,
            parent,
            name: name.into(),
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

    fn tree() -> HashMap<ChannelId, ChannelInfo> {
        [
            ch(1, None, 0, "Lobby"),
            ch(2, None, 1, "Gaming"),
            ch(3, Some(2), 0, "Squad"),
            ch(4, Some(3), 0, "Duo"),
            ch(5, None, 2, "AFK"),
            ch(6, None, 3, "Music"),
        ]
        .into_iter()
        .map(|c| (c.id, c))
        .collect()
    }

    fn to(parent: Option<ChannelId>, position: u32) -> Outcome {
        Outcome::Move(DropTarget { parent, position })
    }

    #[test]
    fn row_thirds_pick_the_zone() {
        assert_eq!(Zone::at(0.1), Zone::Before);
        assert_eq!(Zone::at(0.5), Zone::Into);
        assert_eq!(Zone::at(0.9), Zone::After);
    }

    #[test]
    fn reorders_down_among_siblings() {
        assert_eq!(drop_target(&tree(), 1, 5, Zone::After), to(None, 2));
        assert_eq!(drop_target(&tree(), 1, 6, Zone::Before), to(None, 2));
    }

    #[test]
    fn reorders_up_among_siblings() {
        assert_eq!(drop_target(&tree(), 6, 1, Zone::Before), to(None, 0));
        assert_eq!(drop_target(&tree(), 6, 2, Zone::After), to(None, 2));
    }

    #[test]
    fn moves_under_another_parent() {
        assert_eq!(drop_target(&tree(), 5, 3, Zone::Before), to(Some(2), 0));
        assert_eq!(drop_target(&tree(), 3, 1, Zone::After), to(None, 1));
    }

    #[test]
    fn into_a_leaf_becomes_its_last_child() {
        assert_eq!(drop_target(&tree(), 6, 1, Zone::Into), to(Some(1), 0));
        assert_eq!(drop_target(&tree(), 5, 2, Zone::Into), to(Some(2), 1));
    }

    #[test]
    fn refuses_nesting_past_the_depth_limit() {
        assert_eq!(drop_target(&tree(), 5, 4, Zone::Into), Outcome::NotAllowed(Refusal::TooDeep));
        let mut t = tree();
        t.insert(7, ch(7, Some(1), 0, "Quiet"));
        assert_eq!(drop_target(&t, 3, 7, Zone::Into), Outcome::NotAllowed(Refusal::TooDeep));
        assert_eq!(drop_target(&t, 3, 7, Zone::Before), to(Some(1), 0));
    }

    #[test]
    fn refuses_its_own_subtree() {
        assert_eq!(drop_target(&tree(), 2, 3, Zone::Into), Outcome::NotAllowed(Refusal::IntoItself));
        assert_eq!(drop_target(&tree(), 2, 4, Zone::Before), Outcome::NotAllowed(Refusal::IntoItself));
    }

    #[test]
    fn refuses_permanent_under_temporary() {
        let mut t = tree();
        t.get_mut(&5).unwrap().temporary = true;
        assert_eq!(drop_target(&t, 6, 5, Zone::Into), Outcome::NotAllowed(Refusal::PermanentUnderTemporary));
        t.get_mut(&6).unwrap().temporary = true;
        assert_eq!(drop_target(&t, 6, 5, Zone::Into), to(Some(5), 0));
    }

    #[test]
    fn refuses_a_name_already_under_the_new_parent() {
        let mut t = tree();
        t.insert(7, ch(7, Some(2), 1, "afk"));
        assert_eq!(drop_target(&t, 5, 2, Zone::Into), Outcome::NotAllowed(Refusal::NameTaken));
    }

    #[test]
    fn a_drop_that_changes_nothing_does_nothing() {
        assert_eq!(drop_target(&tree(), 2, 2, Zone::Into), Outcome::Nothing);
        assert_eq!(drop_target(&tree(), 2, 1, Zone::After), Outcome::Nothing);
        assert_eq!(drop_target(&tree(), 2, 5, Zone::Before), Outcome::Nothing);
        assert_eq!(drop_target(&tree(), 4, 3, Zone::Into), Outcome::Nothing);
    }
}
