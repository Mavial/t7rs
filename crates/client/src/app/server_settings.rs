//! The server settings dialog: roles with their permissions and members, and
//! the ban list.

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, vec2};
use t7_proto::model::ServerView;
use t7_proto::perms::{Perm, Perms, Rank, RoleChange, RoleId, Roles};
use t7_proto::{BanInfo, C2S, MAX_ROLE_NAME_LEN};

use super::App;
use super::widgets::{field, hint, list_row, paint_avatar, placeholder, section, text_input};
use crate::theme;
use crate::theme::Btn;

struct DragRole(RoleId);

pub(super) struct ServerSettings {
    pub(super) tab: SettingsTab,
    /// The ban list; `None` until the server sends it.
    pub(super) bans: Option<Vec<BanInfo>>,
    pub(super) picked: Pick,
    name_draft: String,
    new_role: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsTab {
    Roles,
    Bans,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Pick {
    Owner,
    Role(RoleId),
    Everyone,
}

const BODY_H: f32 = 480.0;

fn bans_tab(ui: &mut egui::Ui, bans: Option<&[BanInfo]>, send: &mut Vec<C2S>) {
    match bans {
        None => {
            ui.add(egui::Spinner::new().size(18.0).color(theme::ACCENT_TEXT));
        }
        Some([]) => {
            ui.label(RichText::new("Nobody is banned.").color(theme::TEXT_DIM));
        }
        Some(list) => {
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Frame::new().stroke(Stroke::new(1.0, theme::BORDER)).corner_radius(CornerRadius::same(8)).show(
                    ui,
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (i, ban) in list.iter().enumerate() {
                            if i > 0 {
                                let x = ui.max_rect().x_range();
                                ui.painter().hline(x, ui.cursor().top(), Stroke::new(1.0, theme::BORDER));
                            }
                            if ban_row(ui, ban) {
                                send.push(C2S::Unban { id: ban.id });
                            }
                        }
                    },
                );
            });
        }
    }
}

fn roles_tab(ui: &mut egui::Ui, view: &ServerView, roles: &Roles, s: &mut ServerSettings, send: &mut Vec<C2S>) {
    let mine = view.me().map_or(Rank::EVERYONE, |me| roles.rank(me.identity));
    if let Pick::Role(id) = s.picked
        && roles.role(id).is_none()
    {
        s.picked = Pick::Everyone;
    }
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| role_list(ui, roles, mine, s, send));
        ui.add_space(18.0);
        ui.vertical(|ui| role_details(ui, view, roles, mine, s, send));
    });
}

/// The roles from the owner down to everyone: pick one, drag to rank, or add one.
fn role_list(ui: &mut egui::Ui, roles: &Roles, mine: Rank, s: &mut ServerSettings, send: &mut Vec<C2S>) {
    let below = |role: RoleId| mine > roles.height(role);
    let change = |send: &mut Vec<C2S>, change| send.push(C2S::ChangeRoles(change));
    ui.set_width(160.0);
    ui.spacing_mut().item_spacing.y = 2.0;
    let owner = roles.owner.and_then(|o| roles.members.get(&o)).map_or("nobody yet", |m| m.nickname.as_str());
    if list_row(ui, "Owner", owner, s.picked == Pick::Owner, Sense::click()).clicked() {
        s.picked = Pick::Owner;
    }
    let dragging = egui::DragAndDrop::payload::<DragRole>(ui.ctx()).map(|d| d.0);
    for (position, role) in roles.ranked.iter().enumerate() {
        let holders = roles.members.values().filter(|m| m.roles.contains(&role.id)).count();
        let count = if holders == 0 { String::new() } else { holders.to_string() };
        let picked = s.picked == Pick::Role(role.id);
        let sense = if below(role.id) { Sense::click_and_drag() } else { Sense::click() };
        let resp = list_row(ui, &role.name, &count, picked, sense);
        if resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        if resp.clicked() {
            s.picked = Pick::Role(role.id);
        }
        if below(role.id) {
            resp.dnd_set_drag_payload(DragRole(role.id));
        }
        let below_mine_at_position = mine > roles.height_at(position);
        if let Some(dragged) = dragging.filter(|d| *d != role.id && below_mine_at_position && resp.contains_pointer()) {
            let y = if roles.height(dragged) > roles.height(role.id) { resp.rect.bottom() } else { resp.rect.top() };
            let line = [egui::pos2(resp.rect.left() + 4.0, y), egui::pos2(resp.rect.right() - 4.0, y)];
            ui.painter().line_segment(line, Stroke::new(2.0, theme::ACCENT));
            if resp.dnd_release_payload::<DragRole>().is_some() {
                change(send, RoleChange::Move { id: dragged, position: position as u32 });
            }
        }
    }
    if list_row(ui, "Everyone", "", s.picked == Pick::Everyone, Sense::click()).clicked() {
        s.picked = Pick::Everyone;
    }
    if mine > Rank::EVERYONE {
        ui.add_space(6.0);
        match &mut s.new_role {
            None => {
                if ui.add(Btn::ghost("+  New role").small()).clicked() {
                    s.new_role = Some(String::new());
                }
            }
            Some(name) => {
                let edit = text_input(name).hint_text(placeholder("Name, then Enter")).char_limit(MAX_ROLE_NAME_LEN);
                let field = ui.add(edit);
                if field.lost_focus() {
                    let name = std::mem::take(name).trim().to_owned();
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !name.is_empty() {
                        change(send, RoleChange::Create { name });
                    }
                    s.new_role = None;
                } else if !field.has_focus() {
                    field.request_focus();
                }
            }
        }
    }
}

/// The picked role: its name, permissions and members.
fn role_details(
    ui: &mut egui::Ui,
    view: &ServerView,
    roles: &Roles,
    mine: Rank,
    s: &mut ServerSettings,
    send: &mut Vec<C2S>,
) {
    let below = |role: RoleId| mine > roles.height(role);
    let change = |send: &mut Vec<C2S>, change| send.push(C2S::ChangeRoles(change));
    let height = ui.available_height();
    egui::ScrollArea::vertical().max_height(height).auto_shrink([false, true]).show(ui, |ui| match s.picked {
        Pick::Owner => {
            hint(
                ui,
                "The owner may do everything. Only a claim token from the server's host makes \
                 someone else the owner.",
            );
        }
        Pick::Everyone => {
            hint(ui, "Everyone has this role. It's the baseline for all the others.");
            section(ui, "Permissions");
            perm_switches(ui, roles.everyone, below(RoleId::EVERYONE), |perms| {
                change(send, RoleChange::SetPermissions { id: RoleId::EVERYONE, perms });
            });
        }
        Pick::Role(id) => {
            let role = roles.role(id).expect("checked above");
            let editable = below(id);
            let name_id = egui::Id::unique(("role-name", id));
            if !ui.memory(|m| m.has_focus(name_id)) {
                s.name_draft.clone_from(&role.name);
            }
            let edit = text_input(&mut s.name_draft).id(name_id).char_limit(MAX_ROLE_NAME_LEN);
            let field = ui.add_enabled_ui(editable, |ui| field(ui, "Name", edit)).inner;
            let name = s.name_draft.trim();
            if field.lost_focus() && !name.is_empty() && name != role.name {
                change(send, RoleChange::Rename { id, name: name.to_owned() });
            }
            if id == RoleId::MODERATOR {
                hint(ui, "Channel moderators and owners of temporary channels also hold it, where they moderate.");
            }
            section(ui, "Permissions");
            perm_switches(ui, role.perms, editable, |perms| {
                change(send, RoleChange::SetPermissions { id, perms });
            });
            section(ui, "Members");
            let holders: Vec<_> = roles.members.iter().filter(|(_, m)| m.roles.contains(&id)).collect();
            if holders.is_empty() {
                hint(ui, "Nobody yet. Right-click someone in the channel tree, then Roles.");
            }
            for (identity, member) in holders {
                ui.horizontal(|ui| {
                    ui.set_min_height(32.0);
                    let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                    paint_avatar(ui.painter(), rect.center(), 10.0, &member.nickname);
                    ui.label(RichText::new(&member.nickname).color(theme::TEXT));
                    if !view.clients.values().any(|c| c.identity == *identity) {
                        ui.label(RichText::new("offline").size(11.5).color(theme::TEXT_FAINT));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let may = editable && mine > roles.rank(*identity);
                        if ui.add_enabled(may, Btn::ghost("Remove").small()).clicked() {
                            change(send, RoleChange::Assign { identity: *identity, role: id, held: false });
                        }
                    });
                });
            }
            if editable && id != RoleId::MODERATOR {
                ui.add_space(14.0);
                if ui.add(Btn::danger("Delete role").small()).clicked() {
                    change(send, RoleChange::Delete { id });
                }
            }
        }
    });
}

fn perm_switches(ui: &mut egui::Ui, perms: Perms, enabled: bool, mut changed: impl FnMut(Perms)) {
    ui.add_enabled_ui(enabled, |ui| {
        ui.columns(2, |columns| {
            let per_column = Perm::ALL.len().div_ceil(2);
            for (i, perm) in Perm::ALL.into_iter().enumerate() {
                let ui = &mut columns[i / per_column];
                let mut on = perms.has(perm);
                if theme::switch(ui, &mut on, perm_label(perm)).changed() {
                    let mut new = perms;
                    new.set(perm, on);
                    changed(new);
                }
            }
        });
    });
}

pub(super) fn perm_label(perm: Perm) -> &'static str {
    match perm {
        Perm::View => "View channels",
        Perm::Join => "Join channels",
        Perm::Speak => "Speak",
        Perm::SendMessages => "Send messages",
        Perm::Bypass => "Join full or locked channels",
        Perm::CreateTemporary => "Create temporary channels",
        Perm::ManageChannels => "Manage channels",
        Perm::ManagePermissions => "Manage channel permissions",
        Perm::MoveMembers => "Move members",
        Perm::MuteMembers => "Mute members",
        Perm::Kick => "Kick",
        Perm::Ban => "Ban",
        Perm::ManageRoles => "Manage roles",
    }
}

fn ban_row(ui: &mut egui::Ui, ban: &BanInfo) -> bool {
    let local = |secs: u64, format: &str| {
        chrono::DateTime::from_timestamp(secs as i64, 0)
            .map_or_default(|t| t.with_timezone(&chrono::Local).format(format).to_string())
    };
    let mut length = match ban.expires_unix {
        Some(end) => format!("until {}", local(end, "%b %-d, %H:%M")),
        None => "permanent".to_owned(),
    };
    if ban.ip.is_some() {
        length.push_str(" · IP too");
    }
    let by = format!(" · by {} on {}", ban.banned_by, local(ban.created_unix, "%b %-d"));
    let mut unban = false;
    egui::Frame::new().inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
            paint_avatar(ui.painter(), rect.center(), 14.0, &ban.nickname);
            let who = ui.vertical(|ui| {
                ui.set_width(ui.available_width() - 80.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                let text =
                    |size, color| TextFormat { font_id: FontId::proportional(size), color, ..Default::default() };
                let mut first = LayoutJob::default();
                first.append(&ban.nickname, 0.0, text(13.5, theme::TEXT));
                first.append(&format!("  · {length}"), 0.0, text(11.5, theme::TEXT_FAINT));
                ui.label(first);
                let mut second = LayoutJob::default();
                match ban.reason.as_str() {
                    "" => second.append("No reason given", 0.0, text(12.0, theme::TEXT_FAINT)),
                    reason => second.append(reason, 0.0, text(12.0, theme::TEXT_DIM)),
                }
                second.append(&by, 0.0, text(12.0, theme::TEXT_DIM));
                ui.label(second);
            });
            who.response.on_hover_text(format!("Identity {}", ban.identity));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                unban = ui.add(Btn::secondary("Unban").small()).clicked();
            });
        });
    });
    unban
}

impl App {
    pub(super) fn open_server_settings(&mut self) {
        let Some(view) = &self.view else { return };
        let tab = if view.may(Perm::ManageRoles) { SettingsTab::Roles } else { SettingsTab::Bans };
        if view.may(Perm::Ban) {
            self.net.send(C2S::ListBans);
        }
        let picked = Pick::Role(RoleId::MODERATOR);
        self.server_settings =
            Some(ServerSettings { tab, bans: None, picked, name_draft: String::new(), new_role: None });
    }

    pub(super) fn server_settings_dialog(&mut self, ctx: &egui::Context) {
        let (Some(settings), Some(view)) = (&mut self.server_settings, &self.view) else {
            self.server_settings = None;
            return;
        };
        let tabs: Vec<(SettingsTab, &str)> =
            [(SettingsTab::Roles, "Roles", Perm::ManageRoles), (SettingsTab::Bans, "Bans", Perm::Ban)]
                .into_iter()
                .filter(|(_, _, perm)| view.may(*perm))
                .map(|(tab, label, _)| (tab, label))
                .collect();
        let Some(&(first, _)) = tabs.first() else {
            self.server_settings = None;
            return;
        };
        if !tabs.iter().any(|(tab, _)| *tab == settings.tab) {
            settings.tab = first;
        }
        let mut send = Vec::new();
        let mut done = false;
        let body_h = (ctx.content_rect().height() - 170.0).clamp(160.0, BODY_H);
        let modal = theme::modal("server-settings").show(ctx, |ui| {
            ui.set_width(720.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Server settings").size(17.0).strong().color(theme::TEXT));
                if tabs.len() > 1 {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        theme::segmented(ui, &mut settings.tab, &tabs);
                    });
                }
            });
            ui.add_space(16.0);
            let min_h = if tabs.len() > 1 { body_h } else { 0.0 };
            let size = vec2(ui.available_width(), body_h);
            ui.allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
                ui.set_min_height(min_h);
                match (settings.tab, &view.roles) {
                    (SettingsTab::Roles, Some(roles)) => roles_tab(ui, view, roles, settings, &mut send),
                    (SettingsTab::Roles, None) => {
                        ui.add(egui::Spinner::new().size(18.0).color(theme::ACCENT_TEXT));
                    }
                    (SettingsTab::Bans, _) => bans_tab(ui, settings.bans.as_deref(), &mut send),
                }
            });
            ui.add_space(18.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                done = ui.add(Btn::primary("Done")).clicked();
            });
        });
        for msg in send {
            self.net.send(msg);
        }
        if done || modal.should_close() {
            self.server_settings = None;
        }
    }
}
