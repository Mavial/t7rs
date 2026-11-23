use std::collections::HashMap;

use t7_proto::tree::{self, Misplaced};
use t7_proto::{ChannelId, ChannelInfo, MAX_CHANNEL_DEPTH};

/// Which part of the hovered row the pointer is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Before,
    Into,
    After,
}

impl Zone {
    /// `fraction` is the pointer's height within the row: 0 at the top, 1 at the bottom.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTarget {
    pub parent: Option<ChannelId>,
    pub position: u32,
}

pub fn refusal(why: Misplaced) -> String {
    match why {
        Misplaced::CategoryPlacement => "Categories stay at the top level".into(),
        Misplaced::IntoItself => "A channel can't go inside itself".into(),
        Misplaced::PermanentUnderTemporary => "Permanent channels can't go inside a temporary one".into(),
        Misplaced::TooDeep => format!("Channels nest at most {MAX_CHANNEL_DEPTH} levels deep"),
        Misplaced::HidesDefault => "Everyone must be able to see the default channel".into(),
        Misplaced::NameTaken => "A channel with that name is already there".into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Move(DropTarget),
    /// Dropping here leaves the tree as it is.
    Nothing,
    NotAllowed(Misplaced),
}

/// What dropping `dragged` on `zone` of the row for `hovered` would do.
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
    let siblings: Vec<ChannelId> = tree::children_of(channels, parent).into_iter().filter(|&s| s != dragged).collect();
    let position = match zone {
        Zone::Into => siblings.len(),
        Zone::Before => siblings.iter().position(|&s| s == hovered).unwrap_or(0),
        Zone::After => siblings.iter().position(|&s| s == hovered).map_or(siblings.len(), |i| i + 1),
    };

    if let Err(why) = tree::check_move(channels, moving, parent) {
        return Outcome::NotAllowed(why);
    }
    if parent == moving.parent
        && tree::children_of(channels, parent).iter().position(|&s| s == dragged) == Some(position)
    {
        return Outcome::Nothing;
    }
    Outcome::Move(DropTarget { parent, position: position as u32 })
}

#[cfg(test)]
mod tests {
    use t7_proto::ChannelKind;
    use t7_proto::perms::{Override, Perm, Perms, RoleId};

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
            owner: None,
            moderators: Vec::new(),
            kind: ChannelKind::Voice,
            overrides: Vec::new(),
        }
    }

    /// Lobby(1), Gaming(2) > Squad(3) > Duo(4), AFK(5), Music(6)
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
        // Lobby after AFK: siblings without Lobby are Gaming, AFK, Music.
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
        assert_eq!(drop_target(&tree(), 5, 4, Zone::Into), Outcome::NotAllowed(Misplaced::TooDeep));
        // Squad carries Duo with it, so it can't go under Lobby's child.
        let mut t = tree();
        t.insert(7, ch(7, Some(1), 0, "Quiet"));
        assert_eq!(drop_target(&t, 3, 7, Zone::Into), Outcome::NotAllowed(Misplaced::TooDeep));
        assert_eq!(drop_target(&t, 3, 7, Zone::Before), to(Some(1), 0));
    }

    #[test]
    fn refuses_its_own_subtree() {
        assert_eq!(drop_target(&tree(), 2, 3, Zone::Into), Outcome::NotAllowed(Misplaced::IntoItself));
        assert_eq!(drop_target(&tree(), 2, 4, Zone::Before), Outcome::NotAllowed(Misplaced::IntoItself));
    }

    #[test]
    fn refuses_permanent_under_temporary() {
        let mut t = tree();
        t.get_mut(&5).unwrap().temporary = true;
        assert_eq!(drop_target(&t, 6, 5, Zone::Into), Outcome::NotAllowed(Misplaced::PermanentUnderTemporary));
        t.get_mut(&6).unwrap().temporary = true;
        assert_eq!(drop_target(&t, 6, 5, Zone::Into), to(Some(5), 0));
    }

    #[test]
    fn refuses_a_name_already_under_the_new_parent() {
        let mut t = tree();
        t.insert(7, ch(7, Some(2), 1, "afk"));
        assert_eq!(drop_target(&t, 5, 2, Zone::Into), Outcome::NotAllowed(Misplaced::NameTaken));
    }

    #[test]
    fn categories_stay_at_the_top_and_take_channels_in() {
        let mut t = tree();
        t.get_mut(&5).unwrap().kind = ChannelKind::Category;
        assert_eq!(drop_target(&t, 5, 3, Zone::Before), Outcome::NotAllowed(Misplaced::CategoryPlacement));
        assert_eq!(drop_target(&t, 5, 1, Zone::Into), Outcome::NotAllowed(Misplaced::CategoryPlacement));
        assert_eq!(drop_target(&t, 5, 1, Zone::Before), to(None, 0));
        assert_eq!(drop_target(&t, 6, 5, Zone::Into), to(Some(5), 0));
    }

    #[test]
    fn a_lobby_keeps_a_level_for_its_rooms() {
        let mut t = tree();
        t.get_mut(&6).unwrap().kind = ChannelKind::Lobby;
        assert_eq!(drop_target(&t, 6, 3, Zone::Into), Outcome::NotAllowed(Misplaced::TooDeep));
        assert_eq!(drop_target(&t, 6, 2, Zone::Into), to(Some(2), 1));
    }

    #[test]
    fn refuses_hiding_the_default_channel() {
        let mut t = tree();
        t.get_mut(&1).unwrap().default = true;
        t.get_mut(&6).unwrap().overrides = vec![Override::denying(RoleId::EVERYONE, Perms::of(&[Perm::View]))];
        assert_eq!(drop_target(&t, 1, 6, Zone::Into), Outcome::NotAllowed(Misplaced::HidesDefault));
        assert_eq!(drop_target(&t, 5, 6, Zone::Into), to(Some(6), 0));
    }

    #[test]
    fn a_drop_that_changes_nothing_does_nothing() {
        assert_eq!(drop_target(&tree(), 2, 2, Zone::Into), Outcome::Nothing);
        assert_eq!(drop_target(&tree(), 2, 1, Zone::After), Outcome::Nothing);
        assert_eq!(drop_target(&tree(), 2, 5, Zone::Before), Outcome::Nothing);
        assert_eq!(drop_target(&tree(), 4, 3, Zone::Into), Outcome::Nothing);
    }
}
