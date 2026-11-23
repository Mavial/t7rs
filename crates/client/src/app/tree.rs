//! The channel tree: channels and the people in them, drag and drop, and the
//! right-click menus that manage both.

use std::collections::{BTreeMap, BTreeSet};

use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense, Stroke, vec2};
use t7_proto::model::ServerView;
use t7_proto::perms::{Perm, Rank, RoleChange, RoleId, Roles, Scope};
use t7_proto::{C2S, ChannelId, ChannelInfo, ChannelKind, ClientId, ClientInfo, Identity, MAX_CHANNEL_DEPTH, Speech};

use super::widgets::{Unit, paint_avatar, slider};
use super::{App, Conn, manages};
use crate::channel_drop::{self, DropTarget, Outcome, Zone};
use crate::icons::{self, Icon};
use crate::theme::{self, Btn};

/// Where a row's icon is centered and its name starts, from the row's left
/// edge plus its indent, and the gap kept on the right.
const ICON_X: f32 = 13.0;
const TEXT_X: f32 = 28.0;
const RIGHT_PAD: f32 = 8.0;

/// Something picked in the channel tree, applied after drawing it.
enum TreeAction {
    Join(ChannelId),
    OpenPrivate(ClientId),
    Poke(ClientId),
    SetVolume { id: ClientId, nickname: String, volume: f32 },
    Create(Option<ChannelId>),
    Edit(ChannelId),
    Shift(ChannelId, i32),
    Move(ChannelId, DropTarget),
    MakeDefault(ChannelId),
    Delete(ChannelId),
    ClaimOwnership,
    SetModerator { channel: ChannelId, identity: Identity, moderator: bool },
    Kick { target: ClientId, from_server: bool },
    Ban(ClientId),
    ServerMute { target: ClientId, muted: bool },
    ServerSettings,
    SetRole { identity: Identity, role: RoleId, held: bool },
    MoveMember { target: ClientId, channel: ChannelId },
    Fold(String),
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
    picked: Option<TreeAction>,
}

/// A channel's row, worked out before it is drawn.
struct ChannelRow<'a> {
    ch: &'a ChannelInfo,
    depth: usize,
    /// A folded category: the row stands for every channel below it.
    folded: bool,
    clients: Vec<&'a ClientInfo>,
    manages: bool,
    may_rearrange: bool,
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
}

impl App {
    pub(super) fn channel_tree(&mut self, ui: &mut egui::Ui) {
        let Some(view) = &self.view else { return };
        let folded = match &self.conn {
            Conn::Connected { server } => self.settings.folded.get(server),
            _ => None,
        };
        let tree = Tree {
            view,
            folded,
            volumes: &self.settings.user_volumes,
            speaking: self.voice.playback.speaking(),
            me_talking: self.voice.shared.transmitting(),
            dragging: Dragging::of(ui.ctx()),
            pointer: ui.input(|i| i.pointer.interact_pos()),
            picked: None,
        };
        let frame = egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::symmetric(8, 10));
        let picked = egui::Panel::left("tree")
            .frame(frame)
            .resizable(true)
            .default_size(290.0)
            .size_range(220.0..=520.0)
            .show(ui, |ui| tree.show(ui))
            .inner;
        if let Some(action) = picked {
            self.apply_tree_action(action);
        }
    }

    fn apply_tree_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::Join(ch) => self.join_channel(ch),
            TreeAction::OpenPrivate(id) => self.open_private(id),
            TreeAction::Poke(id) => self.open_poke(id),
            TreeAction::SetVolume { id, nickname, volume } => self.set_user_volume(id, &nickname, volume),
            TreeAction::Create(parent) => self.open_create_channel(parent),
            TreeAction::Edit(ch) => self.open_edit_channel(ch),
            TreeAction::Shift(ch, delta) => self.shift_channel(ch, delta),
            TreeAction::Move(ch, to) => {
                self.net.send(C2S::MoveChannel { id: ch, parent: to.parent, position: to.position });
            }
            TreeAction::MakeDefault(ch) => self.net.send(C2S::SetDefaultChannel { id: ch }),
            TreeAction::Delete(ch) => self.open_delete_channel(ch),
            TreeAction::ClaimOwnership => self.open_claim_ownership(),
            TreeAction::SetModerator { channel, identity, moderator } => {
                self.net.send(C2S::SetModerator { channel, identity, moderator });
            }
            TreeAction::Kick { target, from_server } => self.open_kick(target, from_server),
            TreeAction::Ban(target) => self.open_ban(target),
            TreeAction::ServerMute { target, muted } => self.net.send(C2S::ServerMute { target, muted }),
            TreeAction::ServerSettings => self.open_server_settings(),
            TreeAction::SetRole { identity, role, held } => {
                self.net.send(C2S::ChangeRoles(RoleChange::Assign { identity, role, held }));
            }
            TreeAction::Fold(name) => self.toggle_folded(name),
            TreeAction::MoveMember { target, channel } => self.net.send(C2S::MoveMember { target, channel }),
        }
    }

    fn toggle_folded(&mut self, name: String) {
        let Conn::Connected { server } = &self.conn else { return };
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
        self.header(ui);
        ui.add_space(2.0);
        let view = self.view;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
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
                    ui.add_space(6.0);
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

    /// The CHANNELS heading and its menu of server-wide actions.
    fn header(&mut self, ui: &mut egui::Ui) {
        let view = self.view;
        ui.horizontal(|ui| {
            // Lined up with the channel icons below.
            ui.add_space(6.0);
            ui.label(RichText::new("CHANNELS").size(11.0).color(theme::TEXT_FAINT).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 22.0), Sense::click());
                let color = if resp.hovered() { theme::TEXT } else { theme::TEXT_FAINT };
                if resp.hovered() {
                    ui.painter().rect_filled(rect, CornerRadius::same(5), theme::SURFACE_HOVER);
                }
                icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(16.0, 16.0)), Icon::Dots, color);
                egui::Popup::menu(&resp).show(|ui| {
                    let label = if manages(view, Scope::Server) {
                        Some("Create channel…")
                    } else {
                        view.may(Perm::CreateTemporary).then_some("Create temporary channel…")
                    };
                    if let Some(label) = label
                        && ui.button(label).clicked()
                    {
                        self.picked = Some(TreeAction::Create(None));
                    }
                    if (view.may(Perm::ManageRoles) || view.may(Perm::Ban)) && ui.button("Server settings…").clicked()
                    {
                        self.picked = Some(TreeAction::ServerSettings);
                    }
                    if view.is_owner() {
                        ui.add_enabled(false, egui::Button::new("You own this server"));
                    } else if ui.button("Claim ownership…").clicked() {
                        self.picked = Some(TreeAction::ClaimOwnership);
                    }
                });
            });
        });
    }

    fn channel_row(&self, ch: &'a ChannelInfo, depth: usize, folded: bool) -> ChannelRow<'a> {
        let view = self.view;
        let manages_it = manages(view, Scope::Channel(ch.id));
        let clients = view.clients_in(ch.id);
        let stands_for = |channel| channel == ch.id || (folded && t7_proto::tree::within(view, channel, ch.id));
        let people =
            if folded { view.clients.values().filter(|c| stands_for(c.channel)).count() } else { clients.len() };
        ChannelRow {
            ch,
            depth,
            folded,
            manages: manages_it,
            may_rearrange: manages_it && may_renumber_children(view, ch.parent),
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
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), sense);
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
        row.paint_background(ui.painter(), rect, resp.hovered(), drop_into, insert_at);
        row.paint_label(ui.painter(), rect);

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
                    ui.label(RichText::new(channel_drop::refusal(why)).color(theme::TEXT_DIM));
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

    fn channel_menu(&mut self, ui: &mut egui::Ui, row: &ChannelRow) {
        let (view, ch) = (self.view, row.ch);
        let category = ch.kind == ChannelKind::Category;
        if !category {
            let join = if ch.kind == ChannelKind::Lobby { "Open a room" } else { "Join channel" };
            if ui.add_enabled(!row.is_mine && !row.full, egui::Button::new(join)).clicked() {
                self.picked = Some(TreeAction::Join(ch.id));
                ui.close();
            }
            ui.separator();
        }
        let may_create = view.may_in(ch.id, Perm::CreateTemporary) || row.manages;
        let create = if category { "Create channel…" } else { "Create sub-channel…" };
        if row.depth + 1 < MAX_CHANNEL_DEPTH && may_create && ui.button(create).clicked() {
            self.picked = Some(TreeAction::Create(Some(ch.id)));
            ui.close();
        }
        let edit = if category { "Edit category…" } else { "Edit channel…" };
        let may_edit = view.may_rename(ch.id) || view.may_in(ch.id, Perm::ManagePermissions);
        if may_edit && ui.button(edit).clicked() {
            self.picked = Some(TreeAction::Edit(ch.id));
            ui.close();
        }
        if row.may_rearrange {
            if ui.button("Move up").clicked() {
                self.picked = Some(TreeAction::Shift(ch.id, -1));
            }
            if ui.button("Move down").clicked() {
                self.picked = Some(TreeAction::Shift(ch.id, 1));
            }
        }
        if view.may(Perm::ManageChannels)
            && !ch.default
            && !ch.temporary
            && ch.kind == ChannelKind::Voice
            && ui.button("Make default channel").clicked()
        {
            self.picked = Some(TreeAction::MakeDefault(ch.id));
            ui.close();
        }
        let delete = if category { "Delete category…" } else { "Delete channel…" };
        if row.manages && !ch.default && ui.button(delete).clicked() {
            self.picked = Some(TreeAction::Delete(ch.id));
            ui.close();
        }
    }

    fn member(&mut self, ui: &mut egui::Ui, row: &ChannelRow, c: &ClientInfo) {
        let view = self.view;
        let is_me = c.id == view.my_id;
        let movable = is_me || view.may_in(row.ch.id, Perm::MoveMembers);
        let sense = if movable { Sense::click_and_drag() } else { Sense::click() };
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), sense);
        if resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(6), theme::SURFACE_HOVER.gamma_multiply(0.7));
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
        };
        member.paint(ui.painter(), rect, row.indent());

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
        let (view, c) = (self.view, member.c);
        if ui.button("Send private message").clicked() {
            self.picked = Some(TreeAction::OpenPrivate(c.id));
            ui.close();
        }
        if ui.button("Poke…").clicked() {
            self.picked = Some(TreeAction::Poke(c.id));
            ui.close();
        }
        let kicks = [
            (false, "Kick from channel…", !ch.default && view.may_in(ch.id, Perm::Kick)),
            (true, "Kick from server…", view.may(Perm::Kick)),
        ];
        let ban = view.may(Perm::Ban);
        let mutes = view.may_in(ch.id, Perm::MuteMembers);
        let set_moderator = view.may_in(ch.id, Perm::ManagePermissions);
        let roles = view.roles.as_ref().filter(|_| view.may(Perm::ManageRoles));
        if movable || kicks.iter().any(|k| k.2) || ban || mutes || set_moderator || roles.is_some() {
            ui.separator();
        }
        if movable {
            self.move_to_menu(ui, ch.id, c.id);
        }
        if let Some(roles) = roles {
            self.roles_menu(ui, roles, c);
        }
        for (from_server, label, allowed) in kicks {
            if allowed && ui.button(label).clicked() {
                self.picked = Some(TreeAction::Kick { target: c.id, from_server });
                ui.close();
            }
        }
        if ban && ui.button("Ban…").clicked() {
            self.picked = Some(TreeAction::Ban(c.id));
            ui.close();
        }
        if mutes {
            let muted = c.speech == Speech::ServerMuted;
            let label = if muted { "Unmute on server" } else { "Mute on server" };
            if ui.button(label).clicked() {
                self.picked = Some(TreeAction::ServerMute { target: c.id, muted: !muted });
                ui.close();
            }
        }
        if set_moderator {
            let moderator = ch.moderators.contains(&c.identity);
            let label =
                if moderator { "Remove as moderator".to_owned() } else { format!("Make moderator of {}", ch.name) };
            if ui.button(label).clicked() {
                let identity = c.identity;
                self.picked = Some(TreeAction::SetModerator { channel: ch.id, identity, moderator: !moderator });
                ui.close();
            }
        }
        ui.separator();
        self.volume_control(ui, c, member.volume);
    }

    fn move_to_menu(&mut self, ui: &mut egui::Ui, from: ChannelId, target: ClientId) {
        let view = self.view;
        ui.menu_button("Move to", |ui| {
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for &(depth, to) in view.tree() {
                    let Some(dest) = view.channels.get(&to) else { continue };
                    if dest.kind == ChannelKind::Category {
                        ui.label(RichText::new(dest.name.to_uppercase()).size(11.5));
                        continue;
                    }
                    let label = format!("{}{}", "    ".repeat(depth), dest.name);
                    let may = to != from && view.may_in(to, Perm::MoveMembers);
                    if ui.add_enabled(may, egui::Button::new(label)).clicked() {
                        self.picked = Some(TreeAction::MoveMember { target, channel: to });
                        ui.close();
                    }
                }
            });
        });
    }

    fn roles_menu(&mut self, ui: &mut egui::Ui, roles: &Roles, c: &ClientInfo) {
        let view = self.view;
        ui.menu_button("Roles", |ui| {
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
        ui.label(RichText::new("Volume").size(12.0).color(theme::TEXT_DIM));
        let mut v = volume;
        ui.horizontal(|ui| {
            let size = vec2(200.0, theme::CONTROL_H);
            ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
                slider(ui, &mut v, 0.0..=2.0, Unit::Percent);
            });
            if ui.add_enabled(v != 1.0, Btn::secondary("Reset").small()).clicked() {
                v = 1.0;
            }
        });
        if v != volume {
            self.picked = Some(TreeAction::SetVolume { id: c.id, nickname: c.nickname.clone(), volume: v });
        }
    }
}

impl ChannelRow<'_> {
    fn indent(&self) -> f32 {
        self.depth as f32 * 18.0
    }

    /// The row's fill, the bar marking your channel, and where a dragged channel would land.
    fn paint_background(&self, p: &egui::Painter, rect: Rect, hovered: bool, drop_into: bool, insert_at: Option<Zone>) {
        let bg = if drop_into {
            theme::ACCENT_BG
        } else if self.is_mine {
            theme::SURFACE
        } else if hovered {
            theme::SURFACE_HOVER.gamma_multiply(0.7)
        } else {
            Color32::TRANSPARENT
        };
        p.rect_filled(rect, CornerRadius::same(6), bg);
        if drop_into {
            p.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
        } else if self.is_mine {
            let bar = Rect::from_center_size(egui::pos2(rect.left() + 1.5, rect.center().y), vec2(3.0, 14.0));
            p.rect_filled(bar, CornerRadius::same(2), theme::ACCENT);
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
    fn paint_label(&self, p: &egui::Painter, rect: Rect) {
        let ch = self.ch;
        let indent = self.indent();
        let icon_color = if self.is_mine { theme::ACCENT_TEXT } else { theme::TEXT_FAINT };
        let (icon, icon_size) = match (ch.kind, self.folded) {
            (ChannelKind::Voice, _) => (Icon::Speaker, 15.0),
            (ChannelKind::Lobby, _) => (Icon::SpeakerPlus, 15.0),
            (ChannelKind::Category, false) => (Icon::ChevronDown, 13.0),
            (ChannelKind::Category, true) => (Icon::ChevronRight, 13.0),
        };
        let icon_center = egui::pos2(rect.left() + indent + ICON_X, rect.center().y);
        icons::paint(p, Rect::from_center_size(icon_center, vec2(icon_size, icon_size)), icon, icon_color);
        let name_color = if self.is_mine { theme::TEXT } else { theme::TEXT_DIM };
        let name_pos = egui::pos2(rect.left() + indent + TEXT_X, rect.center().y);
        let name_rect = if ch.kind == ChannelKind::Category {
            p.text(name_pos, Align2::LEFT_CENTER, ch.name.to_uppercase(), FontId::proportional(12.0), name_color)
        } else {
            p.text(name_pos, Align2::LEFT_CENTER, &ch.name, FontId::proportional(13.5), name_color)
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
        if !count.is_empty() {
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

    /// The avatar, the name with its badges and volume note, and the status marks on the right.
    fn paint(&self, p: &egui::Painter, rect: Rect, indent: f32) {
        let c = self.c;
        // The avatar starts where the channel's name does.
        let ax = rect.left() + indent + TEXT_X + 9.0;
        let center = egui::pos2(ax, rect.center().y);
        paint_avatar(p, center, 9.0, &c.nickname);
        if self.talking {
            p.circle_stroke(center, 11.5, Stroke::new(2.0, theme::GREEN));
        }
        let color = if c.status.away { theme::TEXT_FAINT } else { theme::TEXT };
        let name_pos = egui::pos2(ax + 17.0, rect.center().y);
        let name_rect = p.text(name_pos, Align2::LEFT_CENTER, &c.nickname, FontId::proportional(13.5), color);
        let mut name_end = name_rect.right();
        let room_badge = self.owns_room.then_some((Icon::Crown, theme::AMBER));
        for (icon, color) in self.badge.into_iter().chain(room_badge) {
            let r = Rect::from_center_size(egui::pos2(name_end + 11.0, rect.center().y), vec2(12.0, 12.0));
            icons::paint(p, r, icon, color);
            name_end += 16.0;
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
    }

    fn tooltip(&self) -> String {
        let titles: Vec<&str> = (self.c.role.as_deref().into_iter())
            .chain(self.owns_room.then_some("Owns this room"))
            .chain(self.mic().1)
            .collect();
        titles.join("\n")
    }
}

fn may_renumber_children(view: &ServerView, parent: Option<ChannelId>) -> bool {
    manages(view, Scope::within(parent))
}
