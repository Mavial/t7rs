//! The channel tree: channels and the people in them, drag and drop, and the
//! right-click menus that manage both.

use std::collections::{BTreeMap, BTreeSet};

use egui::containers::menu::SubMenu;
use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense, Stroke, vec2};
use t7_proto::model::ServerView;
use t7_proto::perms::{Perm, Rank, RoleChange, RoleId, Roles, Scope};
use t7_proto::{C2S, ChannelId, ChannelInfo, ChannelKind, ClientId, ClientInfo, Identity, MAX_CHANNEL_DEPTH, Speech};

use super::widgets::{Item, Unit, menu_label, menu_separator, menu_style, paint_avatar, slider};
use super::{App, Conn, manages, sharing};
use crate::channel_drop::{self, DropTarget, Outcome, Zone};
use crate::icons::{self, Icon};
use crate::theme::{self, Btn};

/// Where a row's icon is centered and its name starts, from the row's left
/// edge plus its indent, and the gap kept on the right.
const ICON_X: f32 = 15.0;
const TEXT_X: f32 = 30.0;
const RIGHT_PAD: f32 = 8.0;
const INDENT: f32 = 16.0;
const CHANNEL_H: f32 = 30.0;
const MEMBER_H: f32 = 28.0;

/// Something picked in the channel tree, applied after drawing it.
enum TreeAction {
    Join(ChannelId),
    OpenPrivate(ClientId),
    Poke(ClientId),
    SetVolume { id: ClientId, nickname: String, volume: f32 },
    Create(ChannelId),
    Edit(ChannelId),
    Shift(ChannelId, i32),
    Move(ChannelId, DropTarget),
    MakeDefault(ChannelId),
    Delete(ChannelId),
    SetModerator { channel: ChannelId, identity: Identity, moderator: bool },
    Kick { target: ClientId, from_server: bool },
    Ban(ClientId),
    ServerMute { target: ClientId, muted: bool },
    SetRole { identity: Identity, role: RoleId, held: bool },
    MoveMember { target: ClientId, channel: ChannelId },
    Fold(String),
    Watch(ClientId),
    StopWatching,
}

/// Payload while dragging yourself onto another channel.
struct DragSelf;

struct DragMember(ClientId);

struct DragChannel(ChannelId);

/// What is being dragged across the tree this frame.
#[derive(Clone, Copy)]
enum Dragging {
    Nothing,
    Me,
    Member(ClientId),
    Channel(ChannelId),
}

impl Dragging {
    fn of(ctx: &egui::Context) -> Self {
        if egui::DragAndDrop::has_payload_of_type::<DragSelf>(ctx) {
            Self::Me
        } else if let Some(member) = egui::DragAndDrop::payload::<DragMember>(ctx) {
            Self::Member(member.0)
        } else if let Some(channel) = egui::DragAndDrop::payload::<DragChannel>(ctx) {
            Self::Channel(channel.0)
        } else {
            Self::Nothing
        }
    }
}

/// One frame of the tree: what it reads from the app, and what was picked.
struct Tree<'a> {
    view: &'a ServerView,
    /// Categories folded on this server.
    folded: Option<&'a BTreeSet<String>>,
    volumes: &'a BTreeMap<String, f32>,
    speaking: Vec<ClientId>,
    me_talking: bool,
    dragging: Dragging,
    pointer: Option<egui::Pos2>,
    watched: Option<ClientId>,
    picked: Option<TreeAction>,
}

/// A channel's row, worked out before it is drawn.
struct ChannelRow<'a> {
    ch: &'a ChannelInfo,
    depth: usize,
    /// A folded category: the row stands for every channel below it.
    folded: bool,
    clients: Vec<&'a ClientInfo>,
    may_rearrange: bool,
    may_edit: bool,
    may_create: bool,
    may_make_default: bool,
    may_delete: bool,
    is_mine: bool,
    full: bool,
    hidden: bool,
    /// Head count shown on the row; a folded category counts everyone below it.
    people: usize,
}

/// Someone's row under their channel, worked out before it is drawn.
struct MemberRow<'a> {
    c: &'a ClientInfo,
    is_me: bool,
    talking: bool,
    /// A role's shield, or a star where they moderate.
    badge: Option<(Icon, Color32)>,
    owns_room: bool,
    volume: f32,
    watchable: bool,
}

impl App {
    pub(super) fn channel_tree(&mut self, ui: &mut egui::Ui) {
        let Some(view) = self.shown_view() else { return };
        let folded = self.server_key().and_then(|server| self.settings.folded.get(server));
        let lost = matches!(self.conn, Conn::Lost { .. });
        if lost {
            ui.disable();
        }
        let tree = Tree {
            view,
            folded,
            volumes: &self.settings.user_volumes,
            speaking: self.voice.playback.speaking(),
            me_talking: self.voice.shared.transmitting(),
            dragging: Dragging::of(ui.ctx()),
            pointer: ui.input(|i| i.pointer.interact_pos()),
            watched: self.watching.as_ref().map(|w| w.sharer),
            picked: None,
        };
        if let Some(action) = tree.show(ui).filter(|_| !lost) {
            self.apply_tree_action(action);
        }
    }

    fn apply_tree_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::Join(ch) => self.join_channel(ch),
            TreeAction::OpenPrivate(id) => self.open_private(id),
            TreeAction::Poke(id) => self.open_poke(id),
            TreeAction::SetVolume { id, nickname, volume } => self.set_user_volume(id, &nickname, volume),
            TreeAction::Create(parent) => self.open_create_channel(Some(parent)),
            TreeAction::Edit(ch) => self.open_edit_channel(ch),
            TreeAction::Shift(ch, delta) => self.shift_channel(ch, delta),
            TreeAction::Move(ch, to) => {
                self.net.send(C2S::MoveChannel { id: ch, parent: to.parent, position: to.position });
            }
            TreeAction::MakeDefault(ch) => self.net.send(C2S::SetDefaultChannel { id: ch }),
            TreeAction::Delete(ch) => self.open_delete_channel(ch),
            TreeAction::SetModerator { channel, identity, moderator } => {
                self.net.send(C2S::SetModerator { channel, identity, moderator });
            }
            TreeAction::Kick { target, from_server } => self.open_kick(target, from_server),
            TreeAction::Ban(target) => self.open_ban(target),
            TreeAction::ServerMute { target, muted } => self.net.send(C2S::ServerMute { target, muted }),
            TreeAction::SetRole { identity, role, held } => {
                self.net.send(C2S::ChangeRoles(RoleChange::Assign { identity, role, held }));
            }
            TreeAction::Fold(name) => self.toggle_folded(name),
            TreeAction::MoveMember { target, channel } => self.net.send(C2S::MoveMember { target, channel }),
            TreeAction::Watch(sharer) => self.watch(sharer),
            TreeAction::StopWatching => self.stop_watching(),
        }
    }

    fn toggle_folded(&mut self, name: String) {
        let Conn::Connected { server, .. } = &self.conn else { return };
        let folded = self.settings.folded.entry(server.clone()).or_default();
        if !folded.remove(&name) {
            folded.insert(name);
        }
        if folded.is_empty() {
            self.settings.folded.remove(server);
        }
    }

    /// Moves a channel one place up (`-1`) or down (`+1`) among its siblings.
    fn shift_channel(&mut self, channel: ChannelId, delta: i32) {
        let Some(view) = &self.view else { return };
        let Some(ch) = view.channels.get(&channel) else { return };
        let mut siblings: Vec<_> = view.channels.values().filter(|c| c.parent == ch.parent).collect();
        siblings.sort_by_key(|c| (c.order, c.id));
        let Some(index) = siblings.iter().position(|c| c.id == channel) else { return };
        let Some(position) = index.checked_add_signed(delta as isize).filter(|&p| p < siblings.len()) else { return };
        self.net.send(C2S::MoveChannel { id: channel, parent: ch.parent, position: position as u32 });
    }
}

impl<'a> Tree<'a> {
    fn show(mut self, ui: &mut egui::Ui) -> Option<TreeAction> {
        let view = self.view;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut in_folded_category = false;
            for (index, &(depth, id)) in view.tree().iter().enumerate() {
                let ch = &view.channels[&id];
                let category = ch.kind == ChannelKind::Category;
                if depth == 0 {
                    in_folded_category = category && self.folded.is_some_and(|f| f.contains(&ch.name));
                } else if in_folded_category {
                    continue;
                }
                if category && index > 0 {
                    ui.add_space(10.0);
                }
                let row = self.channel_row(ch, depth, in_folded_category);
                self.channel(ui, &row);
                for c in &row.clients {
                    self.member(ui, &row, c);
                }
            }
        });
        self.picked
    }

    fn channel_row(&self, ch: &'a ChannelInfo, depth: usize, folded: bool) -> ChannelRow<'a> {
        let view = self.view;
        let manages_it = manages(view, Scope::Channel(ch.id));
        let clients = view.clients_in(ch.id);
        let stands_for = |channel| channel == ch.id || (folded && view.within(channel, ch.id));
        let people =
            if folded { view.clients.values().filter(|c| stands_for(c.channel)).count() } else { clients.len() };
        ChannelRow {
            ch,
            depth,
            folded,
            may_rearrange: manages_it && may_renumber_children(view, ch.parent),
            may_edit: view.may_rename(ch.id) || view.may_in(ch.id, Perm::ManagePermissions),
            may_create: depth + 1 < MAX_CHANNEL_DEPTH && (view.may_in(ch.id, Perm::CreateTemporary) || manages_it),
            may_make_default: view.may(Perm::ManageChannels)
                && !ch.default
                && !ch.temporary
                && ch.kind == ChannelKind::Voice,
            may_delete: manages_it && !ch.default,
            is_mine: view.my_channel().is_some_and(stands_for),
            full: ch.max_clients.is_some_and(|m| clients.len() as u32 >= m),
            hidden: manages_it && view.hidden(ch.id),
            people,
            clients,
        }
    }

    fn channel(&mut self, ui: &mut egui::Ui, row: &ChannelRow) {
        let ch = row.ch;
        let category = ch.kind == ChannelKind::Category;
        let sense = if row.may_rearrange { Sense::click_and_drag() } else { Sense::click() };
        let height = if category { MEMBER_H } else { CHANNEL_H };
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), sense);
        let channel_drop = self.channel_drop(&resp, rect, ch.id);
        let member_drop = self.member_drop(&resp, ch);
        let dragging_me = matches!(self.dragging, Dragging::Me);
        let drop_into = (dragging_me && resp.contains_pointer() && !row.is_mine && !category)
            || member_drop.is_some()
            || matches!(channel_drop, Some((Zone::Into, Outcome::Move(_))));
        let insert_at = match channel_drop {
            Some((zone @ (Zone::Before | Zone::After), Outcome::Move(_))) => Some(zone),
            _ => None,
        };
        let more_id = resp.id.with("more");
        let more_open = egui::Popup::is_id_open(ui.ctx(), more_id.with("popup"));
        let hot = (resp.contains_pointer() || more_open || resp.context_menu_opened())
            && matches!(self.dragging, Dragging::Nothing)
            && ui.is_enabled();
        row.paint_background(ui.painter(), rect, hot, drop_into, insert_at);
        let more = hot && row.has_menu();
        row.paint_label(ui.painter(), rect, hot, !more);
        if more {
            let center = egui::pos2(rect.right() - RIGHT_PAD - 7.0, rect.center().y);
            let more_rect = Rect::from_center_size(center, vec2(22.0, 22.0));
            let more_resp = ui.interact(more_rect, more_id, Sense::click());
            if more_open || more_resp.hovered() {
                ui.painter().rect_filled(more_rect, CornerRadius::same(5), theme::SURFACE_ACTIVE);
            }
            let dots = Rect::from_center_size(center, vec2(16.0, 16.0));
            icons::paint(ui.painter(), dots, Icon::Dots, if more_open { theme::TEXT } else { theme::TEXT_DIM });
            egui::Popup::menu(&more_resp).show(|ui| self.channel_menu(ui, row));
        }

        // Releasing takes the payload whatever its type, so ask only for the one being dragged.
        if category {
            if resp.clicked() {
                self.picked = Some(TreeAction::Fold(ch.name.clone()));
            }
        } else if resp.double_clicked() || (dragging_me && resp.dnd_release_payload::<DragSelf>().is_some()) {
            self.picked = Some(TreeAction::Join(ch.id));
        } else if let Some(target) = member_drop
            && resp.dnd_release_payload::<DragMember>().is_some()
        {
            self.picked = Some(TreeAction::MoveMember { target, channel: ch.id });
        }
        if row.may_rearrange {
            resp.dnd_set_drag_payload(DragChannel(ch.id));
            if resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
        }
        if let Some((_, outcome)) = channel_drop {
            if let Outcome::NotAllowed(why) = outcome {
                ui.ctx().set_cursor_icon(egui::CursorIcon::NotAllowed);
                egui::Tooltip::for_widget(&resp).at_pointer().show(|ui| {
                    ui.label(RichText::new(why.message()).color(theme::TEXT_DIM));
                });
            }
            if let (Dragging::Channel(dragged), Outcome::Move(target)) = (self.dragging, outcome)
                && resp.dnd_release_payload::<DragChannel>().is_some()
            {
                self.picked = Some(TreeAction::Move(dragged, target));
            }
        }
        let tip = row.tooltip();
        let resp = if tip.is_empty() { resp } else { resp.on_hover_text(tip) };
        resp.context_menu(|ui| self.channel_menu(ui, row));
    }

    /// Where the channel being dragged lands if it is dropped on this row.
    fn channel_drop(&self, resp: &egui::Response, rect: Rect, onto: ChannelId) -> Option<(Zone, Outcome)> {
        let (Dragging::Channel(dragged), Some(pos)) = (self.dragging, self.pointer) else { return None };
        if !resp.contains_pointer() {
            return None;
        }
        let zone = Zone::at((pos.y - rect.top()) / rect.height());
        match channel_drop::drop_target(&self.view.channels, dragged, onto, zone) {
            Outcome::Move(to) if !may_renumber_children(self.view, to.parent) => None,
            outcome => Some((zone, outcome)),
        }
    }

    /// The member being dragged over this row, if they may be moved into it.
    fn member_drop(&self, resp: &egui::Response, ch: &ChannelInfo) -> Option<ClientId> {
        let Dragging::Member(member) = self.dragging else { return None };
        let elsewhere = self.view.clients.get(&member).is_some_and(|c| c.channel != ch.id);
        let joinable = ch.kind != ChannelKind::Category;
        let may = elsewhere && resp.contains_pointer() && joinable && self.view.may_in(ch.id, Perm::MoveMembers);
        may.then_some(member)
    }

    fn entry(&mut self, ui: &mut egui::Ui, item: Item, action: TreeAction) {
        if ui.add(item).clicked() {
            self.picked = Some(action);
            ui.close();
        }
    }

    fn channel_menu(&mut self, ui: &mut egui::Ui, row: &ChannelRow) {
        menu_style(ui, 220.0);
        let ch = row.ch;
        let category = ch.kind == ChannelKind::Category;
        if !category {
            let join = if ch.kind == ChannelKind::Lobby { "Open a room" } else { "Join channel" };
            if ui.add_enabled(!row.is_mine && !row.full, Item::new(Icon::ArrowRight, join)).clicked() {
                self.picked = Some(TreeAction::Join(ch.id));
                ui.close();
            }
        }
        let manage = row.may_edit || row.may_create || row.may_rearrange || row.may_make_default;
        if manage && !category {
            menu_separator(ui);
        }
        if row.may_edit {
            let label = if category { "Edit category…" } else { "Edit channel…" };
            self.entry(ui, Item::new(Icon::Pencil, label), TreeAction::Edit(ch.id));
        }
        if row.may_create {
            let label = if category { "Create channel…" } else { "Create sub-channel…" };
            self.entry(ui, Item::new(Icon::Plus, label), TreeAction::Create(ch.id));
        }
        if row.may_rearrange {
            for (icon, label, delta) in [(Icon::ArrowUp, "Move up", -1), (Icon::ArrowDown, "Move down", 1)] {
                if ui.add(Item::new(icon, label)).clicked() {
                    self.picked = Some(TreeAction::Shift(ch.id, delta));
                }
            }
        }
        if row.may_make_default {
            self.entry(ui, Item::new(Icon::Speaker, "Make default channel"), TreeAction::MakeDefault(ch.id));
        }
        if row.may_delete {
            if manage || !category {
                menu_separator(ui);
            }
            let label = if category { "Delete category…" } else { "Delete channel…" };
            self.entry(ui, Item::new(Icon::Trash, label).danger(), TreeAction::Delete(ch.id));
        }
    }

    fn member(&mut self, ui: &mut egui::Ui, row: &ChannelRow, c: &ClientInfo) {
        let view = self.view;
        let is_me = c.id == view.my_id;
        let movable = is_me || view.may_in(row.ch.id, Perm::MoveMembers);
        let sense = if movable { Sense::click_and_drag() } else { Sense::click() };
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), MEMBER_H), sense);
        let dragged = match self.dragging {
            Dragging::Me => is_me,
            Dragging::Member(id) => id == c.id,
            Dragging::Nothing | Dragging::Channel(_) => false,
        };
        let hot = (resp.hovered() || resp.context_menu_opened()) && !dragged && ui.is_enabled();
        if hot {
            ui.painter().rect_filled(rect, CornerRadius::same(6), theme::SURFACE_HOVER);
        }
        let mut painter = ui.painter().clone();
        if dragged {
            painter.multiply_opacity(0.4);
        }
        let badge = if c.role.is_some() {
            Some((Icon::Shield, theme::ACCENT_TEXT))
        } else if view.moderates(c.identity, row.ch.id) {
            Some((Icon::Star, theme::TEXT_DIM))
        } else {
            None
        };
        let member = MemberRow {
            c,
            is_me,
            talking: self.speaking.contains(&c.id) || (is_me && self.me_talking),
            badge,
            owns_room: row.ch.temporary && row.ch.owner == Some(c.id),
            volume: self.volumes.get(&c.nickname).copied().unwrap_or(1.0),
            watchable: c.sharing && !is_me && view.my_channel() == Some(c.channel),
        };
        if let Some(live) = member.paint(&painter, rect, row.indent()) {
            let id = ui.scope_id().with(("live", c.id));
            if sharing::live_badge_clicked(ui, live, id, &member.live_tip(), member.watchable) {
                self.picked = Some(TreeAction::Watch(c.id));
            }
        }

        let tip = member.tooltip();
        let resp = if tip.is_empty() { resp } else { resp.on_hover_text(tip) };
        if is_me {
            if resp.drag_started() {
                resp.dnd_set_drag_payload(DragSelf);
            }
            if resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            resp.on_hover_text("Drag onto a channel to move");
        } else {
            if resp.double_clicked() {
                self.picked = Some(TreeAction::OpenPrivate(c.id));
            }
            if movable && resp.drag_started() {
                resp.dnd_set_drag_payload(DragMember(c.id));
            }
            if resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            resp.context_menu(|ui| self.member_menu(ui, row.ch, &member, movable));
        }
    }

    fn member_menu(&mut self, ui: &mut egui::Ui, ch: &ChannelInfo, member: &MemberRow, movable: bool) {
        menu_style(ui, 250.0);
        let (view, c) = (self.view, member.c);
        self.volume_control(ui, c, member.volume);
        menu_separator(ui);
        if member.watchable && self.watched == Some(c.id) {
            self.entry(ui, Item::new(Icon::ScreenStop, "Stop watching"), TreeAction::StopWatching);
        } else if member.watchable {
            self.entry(ui, Item::new(Icon::Screen, "Watch screen"), TreeAction::Watch(c.id));
        }
        self.entry(ui, Item::new(Icon::Chat, "Send private message"), TreeAction::OpenPrivate(c.id));
        self.entry(ui, Item::new(Icon::Bell, "Poke…"), TreeAction::Poke(c.id));
        let kicks = [
            (false, "Kick from channel…", !ch.default && view.may_in(ch.id, Perm::Kick)),
            (true, "Kick from server…", view.may(Perm::Kick)),
        ];
        let ban = view.may(Perm::Ban);
        let mutes = view.may_in(ch.id, Perm::MuteMembers);
        let set_moderator = view.may_in(ch.id, Perm::ManagePermissions);
        let roles = view.roles.as_ref().filter(|_| view.may(Perm::ManageRoles));
        if !(movable || kicks.iter().any(|k| k.2) || ban || mutes || set_moderator || roles.is_some()) {
            return;
        }
        menu_separator(ui);
        menu_label(ui, "Moderation");
        if movable {
            self.move_to_menu(ui, ch.id, c.id);
        }
        if let Some(roles) = roles {
            self.roles_menu(ui, roles, c);
        }
        if mutes {
            let muted = c.speech == Speech::ServerMuted;
            let label = if muted { "Unmute on server" } else { "Mute on server" };
            self.entry(ui, Item::new(Icon::MicLocked, label), TreeAction::ServerMute { target: c.id, muted: !muted });
        }
        if set_moderator {
            let moderator = ch.moderators.contains(&c.identity);
            let label =
                if moderator { "Remove as moderator".to_owned() } else { format!("Make moderator of {}", ch.name) };
            let action = TreeAction::SetModerator { channel: ch.id, identity: c.identity, moderator: !moderator };
            self.entry(ui, Item::new(Icon::Star, &label), action);
        }
        for (from_server, label, allowed) in kicks {
            if allowed {
                self.entry(ui, Item::new(Icon::Kick, label).danger(), TreeAction::Kick { target: c.id, from_server });
            }
        }
        if ban {
            self.entry(ui, Item::new(Icon::Ban, "Ban…").danger(), TreeAction::Ban(c.id));
        }
    }

    fn move_to_menu(&mut self, ui: &mut egui::Ui, from: ChannelId, target: ClientId) {
        let view = self.view;
        let resp = ui.add(Item::new(Icon::ArrowRight, "Move to").submenu());
        SubMenu::new().show(ui, &resp, |ui| {
            menu_style(ui, 200.0);
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for &(depth, to) in view.tree() {
                    let Some(dest) = view.channels.get(&to) else { continue };
                    if dest.kind == ChannelKind::Category {
                        menu_label(ui, &dest.name);
                        continue;
                    }
                    let label = format!("{}{}", "    ".repeat(depth), dest.name);
                    let may = to != from && view.may_in(to, Perm::MoveMembers);
                    if ui.add_enabled(may, Item::new(Icon::Speaker, &label)).clicked() {
                        self.picked = Some(TreeAction::MoveMember { target, channel: to });
                        ui.close();
                    }
                }
            });
        });
    }

    fn roles_menu(&mut self, ui: &mut egui::Ui, roles: &Roles, c: &ClientInfo) {
        let view = self.view;
        let resp = ui.add(Item::new(Icon::Shield, "Roles").submenu());
        SubMenu::new().show(ui, &resp, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let mine = view.me().map_or(Rank::EVERYONE, |me| roles.rank(me.identity));
            let held = roles.members.get(&c.identity).map_or(&[][..], |m| &m.roles);
            for role in &roles.ranked {
                let mut holds = held.contains(&role.id);
                let may = mine > roles.height(role.id) && mine > roles.rank(c.identity);
                let check = egui::Checkbox::new(&mut holds, role.name.as_str());
                if ui.add_enabled(may, check).changed() {
                    self.picked = Some(TreeAction::SetRole { identity: c.identity, role: role.id, held: holds });
                }
            }
        });
    }

    /// How loud someone is for you alone.
    fn volume_control(&mut self, ui: &mut egui::Ui, c: &ClientInfo, volume: f32) {
        let mut v = volume;
        let frame = egui::Frame::new().inner_margin(Margin { left: 8, right: 8, top: 6, bottom: 4 });
        frame.show(ui, |ui| {
            ui.label(RichText::new("Volume for you").size(12.0).color(theme::TEXT_DIM));
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let size = vec2(200.0, theme::CONTROL_H);
                ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
                    slider(ui, &mut v, 0.0..=2.0, Unit::Percent);
                });
                if ui.add_enabled(v != 1.0, Btn::secondary("Reset").small()).clicked() {
                    v = 1.0;
                }
            });
        });
        if v != volume {
            self.picked = Some(TreeAction::SetVolume { id: c.id, nickname: c.nickname.clone(), volume: v });
        }
    }
}

impl ChannelRow<'_> {
    fn indent(&self) -> f32 {
        self.depth as f32 * INDENT
    }

    fn has_menu(&self) -> bool {
        self.ch.kind != ChannelKind::Category
            || self.may_edit
            || self.may_create
            || self.may_rearrange
            || self.may_make_default
            || self.may_delete
    }

    /// The row's fill, tinted for your channel, and where a dragged channel would land.
    fn paint_background(&self, p: &egui::Painter, rect: Rect, hot: bool, drop_into: bool, insert_at: Option<Zone>) {
        let category = self.ch.kind == ChannelKind::Category;
        let bg = if drop_into || (self.is_mine && !category) {
            theme::ACCENT_BG
        } else if hot {
            theme::SURFACE_HOVER
        } else {
            Color32::TRANSPARENT
        };
        p.rect_filled(rect, CornerRadius::same(6), bg);
        if drop_into {
            p.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
        }
        if let Some(zone) = insert_at {
            // Indented to the level the channel lands on.
            let y = if zone == Zone::Before { rect.top() - 0.5 } else { rect.bottom() + 0.5 };
            let left = rect.left() + self.indent() + 4.0;
            p.circle_filled(egui::pos2(left, y), 3.0, theme::ACCENT);
            p.line_segment([egui::pos2(left, y), egui::pos2(rect.right() - 4.0, y)], Stroke::new(2.0, theme::ACCENT));
        }
    }

    /// The icon, the name with its hidden, locked and temporary marks, and the head count.
    fn paint_label(&self, p: &egui::Painter, rect: Rect, hot: bool, show_count: bool) {
        let ch = self.ch;
        let indent = self.indent();
        let category = ch.kind == ChannelKind::Category;
        let icon_color = if self.is_mine && !category { theme::ACCENT_TEXT } else { theme::TEXT_FAINT };
        let (icon, icon_size) = match (ch.kind, self.folded) {
            (ChannelKind::Voice, _) => (Icon::Speaker, 14.0),
            (ChannelKind::Lobby, _) => (Icon::SpeakerPlus, 14.0),
            (ChannelKind::Category, false) => (Icon::ChevronDown, 12.0),
            (ChannelKind::Category, true) => (Icon::ChevronRight, 12.0),
        };
        let icon_center = egui::pos2(rect.left() + indent + ICON_X, rect.center().y);
        icons::paint(p, Rect::from_center_size(icon_center, vec2(icon_size, icon_size)), icon, icon_color);
        let name_rect = if category {
            let name_pos = egui::pos2(rect.left() + indent + TEXT_X - 6.0, rect.center().y);
            let color = if hot { theme::TEXT_DIM } else { theme::TEXT_FAINT };
            p.text(name_pos, Align2::LEFT_CENTER, ch.name.to_uppercase(), FontId::proportional(11.0), color)
        } else {
            let name_pos = egui::pos2(rect.left() + indent + TEXT_X, rect.center().y);
            let color = if self.is_mine || hot { theme::TEXT } else { theme::TEXT_DIM };
            p.text(name_pos, Align2::LEFT_CENTER, &ch.name, FontId::proportional(13.5), color)
        };
        let mut after = name_rect.right() + 6.0;
        if self.hidden {
            let r = Rect::from_center_size(egui::pos2(after + 7.0, rect.center().y), vec2(14.0, 14.0));
            icons::paint(p, r, Icon::Eye, theme::TEXT_FAINT);
            after += 19.0;
        }
        if ch.has_password {
            let r = Rect::from_center_size(egui::pos2(after + 6.0, rect.center().y), vec2(12.0, 12.0));
            icons::paint(p, r, Icon::Lock, theme::TEXT_FAINT);
            after += 16.0;
        }
        if ch.temporary {
            let pos = egui::pos2(after, rect.center().y + 0.5);
            p.text(pos, Align2::LEFT_CENTER, "temporary", FontId::proportional(11.0), theme::TEXT_FAINT);
        }

        let people = self.people;
        let count = match ch.max_clients {
            Some(m) => format!("{people}/{m}"),
            None if people > 0 => people.to_string(),
            None => String::new(),
        };
        if show_count && !count.is_empty() {
            let color = if self.full { theme::AMBER } else { theme::TEXT_FAINT };
            let pos = rect.right_center() - vec2(RIGHT_PAD, 0.0);
            p.text(pos, Align2::RIGHT_CENTER, count, FontId::proportional(11.5), color);
        }
    }

    fn tooltip(&self) -> String {
        let ch = self.ch;
        let mut tip: Vec<&str> = Vec::new();
        if !ch.topic.is_empty() {
            tip.push(&ch.topic);
        }
        if ch.default {
            tip.push("New people land here.");
        }
        if ch.temporary {
            tip.push("Temporary: removed once everyone has left.");
        }
        if ch.kind == ChannelKind::Lobby {
            tip.push("Join to open a room of your own in it.");
        }
        if self.hidden {
            tip.push("Hidden: only those allowed to view it see it.");
        }
        tip.join("\n")
    }
}

impl MemberRow<'_> {
    /// The microphone mark after the name, and why it is there when the reason isn't theirs.
    fn mic(&self) -> (Option<(Icon, Color32)>, Option<&'static str>) {
        match (self.c.status.muted, self.c.speech) {
            (_, Speech::ServerMuted) => (Some((Icon::MicLocked, theme::RED)), Some("Muted on the server")),
            (true, _) => (Some((Icon::MicOff, theme::RED)), None),
            (false, Speech::NotHere) => (Some((Icon::MicOff, theme::TEXT_FAINT)), Some("Can't speak here")),
            (false, Speech::Allowed) => (None, None),
        }
    }

    fn paint(&self, p: &egui::Painter, rect: Rect, indent: f32) -> Option<Rect> {
        let c = self.c;
        // The avatar starts where the channel's name does.
        let ax = rect.left() + indent + TEXT_X + 10.0;
        let center = egui::pos2(ax, rect.center().y);
        paint_avatar(p, center, 10.0, &c.nickname);
        if self.talking {
            p.circle_stroke(center, 13.0, Stroke::new(2.0, theme::GREEN));
        }
        let color = if c.status.away { theme::TEXT_FAINT } else { theme::TEXT };
        let name_pos = egui::pos2(ax + 18.0, rect.center().y);
        let name_rect = p.text(name_pos, Align2::LEFT_CENTER, &c.nickname, FontId::proportional(13.5), color);
        let mut name_end = name_rect.right();
        let room_badge = self.owns_room.then_some((Icon::Crown, theme::AMBER));
        for (icon, color) in self.badge.into_iter().chain(room_badge) {
            let r = Rect::from_center_size(egui::pos2(name_end + 11.0, rect.center().y), vec2(12.0, 12.0));
            icons::paint(p, r, icon, color);
            name_end += 16.0;
        }
        let live = c.sharing.then(|| sharing::live_badge(p, egui::pos2(name_end + 6.0, rect.center().y)));
        if let Some(live) = live {
            name_end = live.right();
        }
        let hint = if self.is_me {
            Some("you".to_owned())
        } else if self.volume == 0.0 {
            Some("muted for you".to_owned())
        } else if self.volume != 1.0 {
            Some(format!("{:.0}%", self.volume * 100.0))
        } else {
            None
        };
        if let Some(hint) = hint {
            let pos = egui::pos2(name_end, name_rect.center().y) + vec2(6.0, 0.5);
            p.text(pos, Align2::LEFT_CENTER, hint, FontId::proportional(11.0), theme::TEXT_FAINT);
        }
        let mut x = rect.right() - RIGHT_PAD - 7.0;
        let status = [
            c.status.away.then_some((Icon::Moon, theme::AMBER)),
            c.status.deafened.then_some((Icon::HeadphonesOff, theme::RED)),
            self.mic().0,
        ];
        for (icon, color) in status.into_iter().flatten() {
            icons::paint(p, Rect::from_center_size(egui::pos2(x, rect.center().y), vec2(14.0, 14.0)), icon, color);
            x -= 18.0;
        }
        live
    }

    fn tooltip(&self) -> String {
        let titles: Vec<&str> = (self.c.role.as_deref().into_iter())
            .chain(self.owns_room.then_some("Owns this room"))
            .chain(self.mic().1)
            .collect();
        titles.join("\n")
    }

    fn live_tip(&self) -> String {
        if self.is_me {
            "You're sharing your screen".to_owned()
        } else if self.watchable {
            format!("Watch {}'s screen", self.c.nickname)
        } else {
            "Sharing their screen. Join their channel to watch".to_owned()
        }
    }
}

fn may_renumber_children(view: &ServerView, parent: Option<ChannelId>) -> bool {
    manages(view, Scope::within(parent))
}
