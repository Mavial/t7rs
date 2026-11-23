use std::collections::HashMap;

use crate::perms::{ChannelTree, Perm};
use crate::{ChannelId, ChannelInfo, ChannelKind, MAX_CHANNEL_DEPTH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misplaced {
    CategoryPlacement,
    IntoItself,
    PermanentUnderTemporary,
    TooDeep,
    HidesDefault,
    NameTaken,
}

/// Whether `moving` may go under `parent`, which must exist.
pub fn check_move(tree: &impl ChannelTree, moving: &ChannelInfo, parent: Option<ChannelId>) -> Result<(), Misplaced> {
    if let Some(p) = parent {
        if moving.kind == ChannelKind::Category {
            return Err(Misplaced::CategoryPlacement);
        }
        if within(tree, p, moving.id) {
            return Err(Misplaced::IntoItself);
        }
        if tree.channel(p).is_some_and(|t| t.temporary) && !moving.temporary {
            return Err(Misplaced::PermanentUnderTemporary);
        }
        if depth(tree, p) + height(tree, moving.id) > MAX_CHANNEL_DEPTH {
            return Err(Misplaced::TooDeep);
        }
        let default = tree.channels().find(|c| c.default).map(|c| c.id);
        if hides_from_some_role(tree, p) && default.is_some_and(|d| within(tree, d, moving.id)) {
            return Err(Misplaced::HidesDefault);
        }
    }
    if parent != moving.parent && name_taken(tree, &moving.name, parent, Some(moving.id)) {
        return Err(Misplaced::NameTaken);
    }
    Ok(())
}

/// `parent`'s children in display order.
pub fn children_of(tree: &impl ChannelTree, parent: Option<ChannelId>) -> Vec<ChannelId> {
    let mut kids: Vec<&ChannelInfo> = tree.channels().filter(|c| c.parent == parent).collect();
    kids.sort_by_key(|c| (c.order, c.id));
    kids.into_iter().map(|c| c.id).collect()
}

/// Whether `id` is `ancestor` or below it.
pub fn within(tree: &impl ChannelTree, id: ChannelId, ancestor: ChannelId) -> bool {
    let mut at = tree.channel(id);
    while let Some(c) = at {
        if c.id == ancestor {
            return true;
        }
        at = c.parent.and_then(|p| tree.channel(p));
    }
    false
}

/// Top level is depth 1.
pub fn depth(tree: &impl ChannelTree, id: ChannelId) -> usize {
    let mut depth = 1;
    let mut at = tree.channel(id).and_then(|c| c.parent);
    while let Some(p) = at {
        depth += 1;
        at = tree.channel(p).and_then(|c| c.parent);
    }
    depth
}

fn height(tree: &impl ChannelTree, id: ChannelId) -> usize {
    let below = children_of(tree, Some(id)).into_iter().map(|k| height(tree, k)).max().unwrap_or(0);
    (1 + below).max(tree.channel(id).map_or(1, |c| c.kind.levels_including_rooms()))
}

/// Whether `id` or a channel above it denies some role View.
pub fn hides_from_some_role(tree: &impl ChannelTree, id: ChannelId) -> bool {
    let mut at = tree.channel(id);
    while let Some(c) = at {
        if c.overrides.iter().any(|o| o.deny().has(Perm::View)) {
            return true;
        }
        at = c.parent.and_then(|p| tree.channel(p));
    }
    false
}

pub fn name_taken(tree: &impl ChannelTree, name: &str, parent: Option<ChannelId>, except: Option<ChannelId>) -> bool {
    tree.channels().any(|c| c.parent == parent && Some(c.id) != except && c.name.eq_ignore_ascii_case(name))
}

impl ChannelTree for HashMap<ChannelId, ChannelInfo> {
    fn channel(&self, id: ChannelId) -> Option<&ChannelInfo> {
        self.get(&id)
    }

    fn channels(&self) -> impl Iterator<Item = &ChannelInfo> {
        self.values()
    }
}
