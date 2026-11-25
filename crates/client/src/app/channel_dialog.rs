//! Dialogs about one channel: creating or editing it and its permissions,
//! joining with a password, deleting it, and claiming the server.

use egui::{Align, Color32, CornerRadius, FontId, Layout, RichText, Sense, Stroke, vec2};
use t7_proto::model::ServerView;
use t7_proto::perms::{Override, Perm, Perms, RoleId, Scope, Setting};
use t7_proto::{
    C2S, ChannelId, ChannelInfo, ChannelKind, ChannelSettings, Identity, MAX_CHANNEL_DEPTH, MAX_CHANNEL_NAME_LEN,
    MAX_CHANNEL_TOPIC_LEN, PasswordChange,
};

use super::server_settings::perm_label;
use super::widgets::{
    described_switch, dialog_footer, dialog_header, field, hint, paint_avatar, placeholder, row, section, text_input,
};
use super::{App, default_channel_name, manages};
use crate::icons::Icon;
use crate::theme::Btn;
use crate::{icons, theme};

/// The channel-related dialog on screen, if any (one at a time).
pub(super) enum ChannelDialog {
    Form(ChannelForm),
    Password { channel: ChannelId, name: String, password: String, focused: bool },
    Delete { channel: ChannelId, name: String, below: Vec<String>, occupants: usize },
    ClaimOwnership { token: String, focused: bool },
}

/// Creating (`editing: None`) or editing a channel.
pub(super) struct ChannelForm {
    editing: Option<ChannelId>,
    parent: Option<ChannelId>,
    pub(super) kind: ChannelKind,
    pub(super) tab: FormTab,
    pub(super) name: String,
    topic: String,
    pub(super) limited: bool,
    limit: u32,
    /// Editing a channel that already has a password: keep protection on?
    had_password: bool,
    protected: bool,
    password: String,
    temporary: bool,
    focused: bool,
    pub(super) overrides: Vec<Override>,
    permissions_role: RoleId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FormTab {
    Channel,
    Permissions,
}

fn override_of(list: &[Override], role: RoleId) -> Override {
    list.iter().find(|o| o.role == role).copied().unwrap_or(Override::inheriting(role))
}

impl ChannelForm {
    fn roles_touched(&self, saved: &[Override]) -> std::collections::BTreeSet<RoleId> {
        self.overrides.iter().chain(saved).map(|o| o.role).collect()
    }

    fn override_changes(&self, channel: ChannelId, saved: &[Override]) -> Vec<C2S> {
        let roles = self.roles_touched(saved);
        let changed = roles
            .into_iter()
            .map(|role| override_of(&self.overrides, role))
            .filter(|o| *o != override_of(saved, o.role));
        changed.map(|o| C2S::SetOverride { channel, role: o.role, allow: o.allow(), deny: o.deny() }).collect()
    }

    fn changed_settings(&self, saved: &[Override]) -> usize {
        let roles = self.roles_touched(saved);
        let differ = |role| {
            let (now, was) = (override_of(&self.overrides, role), override_of(saved, role));
            Perms::CHANNEL.iter().filter(|&perm| now.get(perm) != was.get(perm)).count()
        };
        roles.into_iter().map(differ).sum()
    }

    fn settings(&self) -> ChannelSettings {
        let password = if self.had_password && !self.protected {
            PasswordChange::Remove
        } else if self.password.is_empty() {
            PasswordChange::Keep
        } else {
            PasswordChange::Set(self.password.clone())
        };
        ChannelSettings {
            name: self.name.trim().to_owned(),
            topic: self.topic.trim().to_owned(),
            max_clients: self.limited.then_some(self.limit),
            password,
        }
    }
}

fn kind_note(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Voice => "A place to talk. People join it from the tree.",
        ChannelKind::Lobby => "Joining it opens a temporary room in it that you own. Nobody stays in the lobby itself.",
        ChannelKind::Category => "A heading for the channels in it, at the top of the tree. Nobody joins it.",
    }
}

/// The channel tab: name, topic, password and size limit. A category or lobby
/// has only the first two, and only those who manage the channel change the
/// last two.
fn channel_fields(ui: &mut egui::Ui, form: &mut ChannelForm, cx: &FormContext) {
    let name = field(
        ui,
        "Name",
        text_input(&mut form.name).hint_text(placeholder("e.g. Squad")).char_limit(MAX_CHANNEL_NAME_LEN),
    );
    if !form.focused {
        name.request_focus();
        form.focused = true;
    }
    let topic = text_input(&mut form.topic).hint_text(placeholder("Optional")).char_limit(MAX_CHANNEL_TOPIC_LEN);
    field(ui, "Topic", topic);
    if form.kind != ChannelKind::Voice {
        if form.editing.is_some() {
            hint(ui, kind_note(form.kind));
        }
        return;
    }
    ui.add_enabled_ui(cx.may_manage, |ui| {
        if form.had_password {
            theme::switch(ui, &mut form.protected, "Password protected");
        }
        if !form.had_password || form.protected {
            let (label, note) = if form.had_password {
                ("New password", "Empty keeps the current one")
            } else {
                ("Password", "Optional")
            };
            field(ui, label, text_input(&mut form.password).password(true).hint_text(placeholder(note)));
        }
        ui.add_space(4.0);
        theme::switch(ui, &mut form.limited, "Limit how many can join");
        if form.limited {
            ui.horizontal(|ui| {
                let limit = egui::DragValue::new(&mut form.limit).range(1..=1000);
                ui.add_sized(vec2(70.0, theme::CONTROL_H), limit);
                let color = if ui.is_enabled() { theme::TEXT_DIM } else { theme::TEXT_FAINT };
                ui.label(RichText::new("people").color(color));
            });
            ui.add_space(4.0);
        }
    });
    if form.editing.is_none() {
        if cx.may_create_permanent {
            described_switch(ui, &mut form.temporary, "Temporary", "Deleted when the last person leaves.");
        } else {
            hint(ui, "Temporary: deleted when the last person leaves. You'll move into it.");
        }
    }
}

fn permissions_tab(ui: &mut egui::Ui, view: &ServerView, channel: ChannelId, form: &mut ChannelForm) {
    let (Some(roles), Some(saved)) = (&view.roles, view.channels.get(&channel)) else {
        ui.add(egui::Spinner::new().size(18.0).color(theme::ACCENT_TEXT));
        return;
    };
    if form.permissions_role != RoleId::EVERYONE && roles.role(form.permissions_role).is_none() {
        form.permissions_role = RoleId::EVERYONE;
    }
    let mine = view.my_rank_in(channel);
    let name = |role: RoleId| roles.role(role).map_or("Everyone", |r| r.name.as_str());
    row(ui, "Role", |ui| {
        let combo = egui::ComboBox::from_id_salt("override-role").selected_text(name(form.permissions_role));
        combo.width(ui.available_width()).show_ui(ui, |ui| {
            for role in roles.ranked.iter().map(|r| r.id).chain([RoleId::EVERYONE]) {
                let set = form.overrides.iter().any(|o| o.role == role);
                let label = if set { format!("{}  •", name(role)) } else { name(role).to_owned() };
                ui.selectable_value(&mut form.permissions_role, role, label);
            }
        });
    });
    let editable = mine > roles.height(form.permissions_role);
    ui.add_space(4.0);
    if editable {
        hint(ui, "A dash inherits from the channel above. The grey word is what that gives.");
    } else {
        hint(ui, "Only roles below yours can be changed.");
    }
    ui.add_space(6.0);

    let here = ChannelInfo { overrides: form.overrides.clone(), ..saved.clone() };
    let inherited = roles.inherited(form.permissions_role, &here, view);
    let was = override_of(&saved.overrides, form.permissions_role);
    let before = form.overrides.iter().find(|o| o.role == form.permissions_role).copied();
    let mut set = before.unwrap_or(Override::inheriting(form.permissions_role));
    ui.add_enabled_ui(editable, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for perm in Perms::CHANNEL.iter() {
            let mut setting = set.get(perm);
            let line = ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                ui.label(RichText::new(perm_label(perm)).color(theme::TEXT));
                let (dot, _) = ui.allocate_exact_size(vec2(6.0, 6.0), Sense::hover());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    tri_state(ui, &mut setting, perm);
                    if setting == Setting::Inherit {
                        let gives = if inherited.has(perm) { "allowed" } else { "denied" };
                        ui.label(RichText::new(gives).size(11.5).color(theme::TEXT_FAINT));
                    }
                });
                dot
            });
            let rect = line.response.rect;
            let p = ui.painter();
            p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
            if setting != was.get(perm) {
                p.circle_filled(line.inner.center(), 3.0, theme::ACCENT_TEXT);
            }
            set.set(perm, setting);
        }
    });
    if Some(set) != before && !(before.is_none() && set.is_empty()) {
        form.overrides.retain(|o| o.role != form.permissions_role);
        if !set.is_empty() {
            form.overrides.push(set);
        }
    }
}

fn tri_state(ui: &mut egui::Ui, setting: &mut Setting, perm: Perm) {
    let options = [
        (Setting::Deny, Icon::Cross, theme::RED, "Deny"),
        (Setting::Inherit, Icon::Dash, theme::TEXT, "Inherit"),
        (Setting::Allow, Icon::Check, theme::GREEN, "Allow"),
    ];
    let segment = vec2(30.0, 22.0);
    let (track, _) = ui.allocate_exact_size(vec2(segment.x * 3.0 + 4.0, segment.y + 4.0), Sense::hover());
    ui.painter().rect(
        track,
        CornerRadius::same(theme::RADIUS),
        theme::SURFACE,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Inside,
    );
    for (i, (option, icon, color, tip)) in options.into_iter().enumerate() {
        let min = egui::pos2(track.left() + 2.0 + segment.x * i as f32, track.top() + 2.0);
        let rect = egui::Rect::from_min_size(min, segment);
        let resp = ui.interact(rect, ui.scope_id().with((perm.key(), i)), Sense::click()).on_hover_text(tip);
        if resp.clicked() {
            *setting = option;
        }
        let selected = *setting == option;
        let fill = match (selected, option) {
            (true, Setting::Inherit) => theme::SURFACE_ACTIVE,
            (true, _) => color.gamma_multiply(0.2),
            (false, _) if resp.hovered() => theme::SURFACE_HOVER,
            (false, _) => Color32::TRANSPARENT,
        };
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(theme::RADIUS - 2), fill);
        let ink = if selected { color } else { theme::TEXT_FAINT };
        icons::paint(p, egui::Rect::from_center_size(rect.center(), vec2(14.0, 14.0)), icon, ink);
    }
}

fn moderator_list(ui: &mut egui::Ui, kind: ChannelKind, list: &[(Identity, Option<String>)]) -> Option<Identity> {
    let joinable = kind != ChannelKind::Category;
    if !joinable && list.is_empty() {
        return None;
    }
    section(ui, "Moderators");
    let mut removed = None;
    if list.is_empty() {
        hint(ui, "Nobody yet. Right-click someone in this channel to make them a moderator.");
    }
    for (identity, nickname) in list {
        ui.horizontal(|ui| {
            ui.set_min_height(32.0);
            if let Some(name) = nickname {
                let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                paint_avatar(ui.painter(), rect.center(), 10.0, name);
                ui.label(RichText::new(name).color(theme::TEXT));
            } else {
                ui.label(RichText::new(identity.to_string()).font(FontId::monospace(12.5)).color(theme::TEXT_DIM));
                ui.label(RichText::new("offline").size(11.5).color(theme::TEXT_FAINT));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(Btn::ghost("Remove").small()).clicked() {
                    removed = Some(*identity);
                }
            });
        });
    }
    removed
}

fn delete_note(below: &[String], occupants: usize, default: &str) -> String {
    let channels = match below {
        [] => String::new(),
        [one] => format!("{one} is deleted with it. "),
        [a, b] => format!("{a} and {b} are deleted with it. "),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more are deleted with it. ", rest.len()),
    };
    let people = match occupants {
        0 if below.is_empty() => "Nobody is in it.".to_owned(),
        0 => String::new(),
        1 => format!("The person inside moves to {default}."),
        n => format!("The {n} people inside move to {default}."),
    };
    format!("{channels}{people}").trim_end().to_owned()
}

impl App {
    pub(super) fn join_channel(&mut self, channel: ChannelId) {
        let Some(view) = &self.view else { return };
        let Some(ch) = view.channels.get(&channel) else { return };
        if view.my_channel() == Some(channel) {
            return;
        }
        if ch.has_password && !view.may_in(channel, Perm::Bypass) {
            let name = ch.name.clone();
            self.channel_dialog =
                Some(ChannelDialog::Password { channel, name, password: String::new(), focused: false });
        } else {
            self.net.send(C2S::JoinChannel { channel, password: None });
        }
    }

    pub(super) fn open_create_channel(&mut self, parent: Option<ChannelId>) {
        let parent_temporary =
            parent.and_then(|p| self.view.as_ref()?.channels.get(&p).map(|c| c.temporary)).unwrap_or(false);
        let permanent = self.view.as_ref().is_some_and(|v| manages(v, Scope::within(parent)));
        self.channel_dialog = Some(ChannelDialog::Form(ChannelForm {
            editing: None,
            parent,
            kind: ChannelKind::Voice,
            tab: FormTab::Channel,
            name: String::new(),
            topic: String::new(),
            limited: false,
            limit: 5,
            had_password: false,
            protected: false,
            password: String::new(),
            temporary: !permanent || parent_temporary,
            focused: false,
            overrides: Vec::new(),
            permissions_role: RoleId::EVERYONE,
        }));
    }

    pub(super) fn open_edit_channel(&mut self, channel: ChannelId) {
        let Some(view) = &self.view else { return };
        let Some(ch) = view.channels.get(&channel) else { return };
        let tab = if view.may_rename(channel) { FormTab::Channel } else { FormTab::Permissions };
        self.channel_dialog = Some(ChannelDialog::Form(ChannelForm {
            editing: Some(channel),
            parent: ch.parent,
            kind: ch.kind,
            tab,
            name: ch.name.clone(),
            topic: ch.topic.clone(),
            limited: ch.max_clients.is_some(),
            limit: ch.max_clients.unwrap_or(5),
            had_password: ch.has_password,
            protected: ch.has_password,
            password: String::new(),
            temporary: ch.temporary,
            focused: false,
            overrides: ch.overrides.clone(),
            permissions_role: RoleId::EVERYONE,
        }));
    }

    pub(super) fn open_claim_ownership(&mut self) {
        self.channel_dialog = Some(ChannelDialog::ClaimOwnership { token: String::new(), focused: false });
    }

    pub(super) fn open_delete_channel(&mut self, channel: ChannelId) {
        let Some(view) = &self.view else { return };
        let Some(ch) = view.channels.get(&channel) else { return };
        let mut all = vec![channel];
        let mut i = 0;
        while i < all.len() {
            let parent = all[i];
            all.extend(view.tree().iter().map(|(_, id)| *id).filter(|id| view.channels[id].parent == Some(parent)));
            i += 1;
        }
        let occupants = view.clients.values().filter(|c| all.contains(&c.channel)).count();
        let below = all[1..].iter().map(|id| view.channels[id].name.clone()).collect();
        self.channel_dialog = Some(ChannelDialog::Delete { channel, name: ch.name.clone(), below, occupants });
    }

    pub(super) fn channel_dialog(&mut self, ctx: &egui::Context) {
        // A dialog about a channel that just disappeared (deleted, or an emptied
        // temporary channel) closes by itself.
        let about = match &self.channel_dialog {
            Some(ChannelDialog::Form(f)) => f.editing.or(f.parent),
            Some(ChannelDialog::Password { channel, .. } | ChannelDialog::Delete { channel, .. }) => Some(*channel),
            _ => None,
        };
        let gone = about.is_some_and(|id| !self.view.as_ref().is_some_and(|v| v.channels.contains_key(&id)));
        if gone {
            self.channel_dialog = None;
        }
        let Some(dialog) = &mut self.channel_dialog else { return };
        let view = self.view.as_ref();
        let form_context = match dialog {
            ChannelDialog::Form(form) => Some(FormContext::new(view, form)),
            _ => None,
        };
        let mut reply = Reply::default();
        let modal = theme::modal("channel-dialog").show(ctx, |ui| {
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            match dialog {
                ChannelDialog::Form(form) => {
                    ui.set_width(440.0);
                    let cx = form_context.as_ref().expect("made for the form");
                    channel_form(ui, view, form, cx, enter, &mut reply);
                }
                ChannelDialog::Password { channel, name, password, focused } => {
                    ui.set_width(380.0);
                    dialog_header(ui, &format!("Join {name}"), "This channel has a password.");
                    let edit = field(ui, "Password", text_input(password).password(true));
                    if !*focused {
                        edit.request_focus();
                        *focused = true;
                    }
                    let ready = !password.is_empty();
                    dialog_footer(ui, |ui| {
                        if ui.add_enabled(ready, Btn::primary("Join")).clicked() || (ready && enter) {
                            reply.send.push(C2S::JoinChannel { channel: *channel, password: Some(password.clone()) });
                        }
                        reply.close |= ui.add(Btn::secondary("Cancel")).clicked();
                    });
                }
                ChannelDialog::Delete { channel, name, below, occupants } => {
                    ui.set_width(380.0);
                    let default = view.map_or_else(|| "the default channel".to_owned(), default_channel_name);
                    dialog_header(ui, &format!("Delete {name}?"), &delete_note(below, *occupants, &default));
                    dialog_footer(ui, |ui| {
                        if ui.add(Btn::danger("Delete")).clicked() {
                            reply.send.push(C2S::DeleteChannel { id: *channel });
                        }
                        reply.close |= ui.add(Btn::secondary("Cancel")).clicked();
                    });
                }
                ChannelDialog::ClaimOwnership { token, focused } => {
                    ui.set_width(420.0);
                    claim_ownership(ui, token, focused, enter, &mut reply);
                }
            }
        });
        if let Some(message) = reply.now {
            self.net.send(message);
        }
        if !reply.send.is_empty() {
            for msg in reply.send {
                self.net.send(msg);
            }
            reply.close = true;
        }
        if reply.close || modal.should_close() {
            self.channel_dialog = None;
        }
    }
}

/// What a frame of the channel dialog asks for.
#[derive(Default)]
struct Reply {
    /// Sent, and then the dialog closes.
    send: Vec<C2S>,
    /// Sent while the dialog stays open.
    now: Option<C2S>,
    close: bool,
}

/// What the channel form offers, worked out from the view before drawing it.
struct FormContext {
    may_create_permanent: bool,
    may_rename: bool,
    may_manage: bool,
    saved_overrides: Option<Vec<Override>>,
    /// The channel's moderators, with nicknames for those online; `None`
    /// unless you may change them, which also adds the Permissions tab.
    moderators: Option<Vec<(Identity, Option<String>)>>,
    parent_name: Option<String>,
    /// The kinds a new channel may be; empty when there is no choice.
    kinds: Vec<(ChannelKind, &'static str)>,
}

impl FormContext {
    fn new(view: Option<&ServerView>, form: &ChannelForm) -> Self {
        let (parent, editing) = (form.parent, form.editing);
        let may_create_permanent = view.is_some_and(|v| manages(v, Scope::within(parent)));
        let saved_overrides = view.zip(editing).and_then(|(v, id)| v.channels.get(&id)).map(|c| c.overrides.clone());
        let moderators = view
            .zip(editing)
            .filter(|(v, id)| v.may_in(*id, Perm::ManagePermissions))
            .and_then(|(v, id)| v.channels.get(&id).map(|ch| (v, ch)))
            .map(|(v, ch)| {
                let online = |m: &Identity| v.clients.values().find(|c| c.identity == *m).map(|c| c.nickname.clone());
                ch.moderators.iter().map(|m| (*m, online(m))).collect()
            });
        let parent_name = match editing {
            None => parent.and_then(|id| view?.channels.get(&id).map(|c| c.name.clone())),
            Some(_) => None,
        };
        let kinds = match (view, editing) {
            (Some(v), None) if may_create_permanent => {
                let above = parent.and_then(|p| v.tree().iter().find(|(_, id)| *id == p));
                let parent_depth = above.map_or(0, |(d, _)| d + 1);
                let lobby_fits = parent_depth + ChannelKind::Lobby.levels_including_rooms() <= MAX_CHANNEL_DEPTH;
                let in_temporary = parent.and_then(|p| v.channels.get(&p)).is_some_and(|c| c.temporary);
                let lobby = (lobby_fits && !in_temporary).then_some((ChannelKind::Lobby, "Lobby"));
                let category = parent.is_none().then_some((ChannelKind::Category, "Category"));
                [Some((ChannelKind::Voice, "Channel")), lobby, category].into_iter().flatten().collect()
            }
            _ => Vec::new(),
        };
        Self {
            may_create_permanent,
            may_rename: editing.is_none_or(|id| view.is_some_and(|v| v.may_rename(id))),
            may_manage: editing.is_none_or(|id| view.is_some_and(|v| v.may_in(id, Perm::ManageChannels))),
            saved_overrides,
            moderators,
            parent_name,
            kinds,
        }
    }
}

/// Creating or editing a channel: its settings, or its permissions and moderators.
fn channel_form(
    ui: &mut egui::Ui,
    view: Option<&ServerView>,
    form: &mut ChannelForm,
    cx: &FormContext,
    enter: bool,
    reply: &mut Reply,
) {
    let (noun, general) = match form.kind {
        ChannelKind::Voice => ("channel", "Channel"),
        ChannelKind::Category => ("category", "Category"),
        ChannelKind::Lobby => ("lobby", "Lobby"),
    };
    let title = match (&form.editing, &cx.parent_name) {
        (Some(_), _) => format!("Edit {}", form.name),
        (None, Some(parent)) => format!("New {noun} in {parent}"),
        (None, None) => format!("New {noun}"),
    };
    dialog_header(ui, &title, "");
    if cx.moderators.is_some() {
        theme::segmented(ui, &mut form.tab, &[(FormTab::Channel, general), (FormTab::Permissions, "Permissions")]);
        ui.add_space(14.0);
    } else if cx.kinds.len() > 1 {
        theme::segmented(ui, &mut form.kind, &cx.kinds);
        ui.add_space(2.0);
        hint(ui, kind_note(form.kind));
        ui.add_space(12.0);
    }
    let saved = cx.saved_overrides.as_deref().unwrap_or_default();
    match (form.tab, view.zip(form.editing)) {
        (FormTab::Permissions, Some((view, channel))) => {
            permissions_tab(ui, view, channel, form);
            if let Some(list) = &cx.moderators
                && let Some(identity) = moderator_list(ui, form.kind, list)
            {
                reply.now = Some(C2S::SetModerator { channel, identity, moderator: false });
            }
        }
        _ => {
            ui.add_enabled_ui(cx.may_rename, |ui| {
                channel_fields(ui, form, cx);
                if cx.may_rename && !cx.may_manage {
                    hint(ui, "As its moderator you can change the name and topic.");
                }
            });
        }
    }
    let ready = !form.name.trim().is_empty();
    let changes = if form.tab == FormTab::Permissions { form.changed_settings(saved) } else { 0 };
    dialog_footer(ui, |ui| {
        let label = if form.editing.is_some() { "Save" } else { "Create" };
        if ui.add_enabled(ready, Btn::primary(label)).clicked() || (ready && enter) {
            let settings = form.settings();
            match form.editing {
                Some(id) => {
                    if cx.may_rename {
                        reply.send.push(C2S::EditChannel { id, settings });
                    }
                    reply.send.extend(form.override_changes(id, saved));
                }
                None => reply.send.push(C2S::CreateChannel {
                    parent: form.parent,
                    settings,
                    temporary: form.temporary && form.kind == ChannelKind::Voice,
                    kind: form.kind,
                }),
            }
        }
        reply.close |= ui.add(Btn::secondary("Cancel")).clicked();
        if changes > 0 {
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let text = if changes == 1 { "1 change".to_owned() } else { format!("{changes} changes") };
                ui.label(RichText::new(text).size(12.0).color(theme::TEXT_FAINT));
            });
        }
    });
}

fn claim_ownership(ui: &mut egui::Ui, token: &mut String, focused: &mut bool, enter: bool, reply: &mut Reply) {
    dialog_header(
        ui,
        "Claim ownership",
        "The owner can do everything and hands out roles. The server prints a claim token in its log until \
         someone claims it. It works once.",
    );
    let edit = text_input(token).password(true).hint_text(placeholder("Paste the token from the server log"));
    let edit = field(ui, "Claim token", edit);
    if !*focused {
        edit.request_focus();
        *focused = true;
    }
    let ready = !token.trim().is_empty();
    dialog_footer(ui, |ui| {
        if ui.add_enabled(ready, Btn::primary("Claim")).clicked() || (ready && enter) {
            reply.send.push(C2S::ClaimOwnership { token: token.trim().to_owned() });
        }
        reply.close |= ui.add(Btn::secondary("Cancel")).clicked();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_sends_only_the_roles_whose_settings_changed() {
        let view = Perms::of(&[Perm::View]);
        let saved = [
            Override::denying(RoleId::EVERYONE, view),
            Override::allowing(RoleId::MODERATOR, view),
            Override::allowing(RoleId::ADMIN, view),
        ];
        let mut form = ChannelForm {
            editing: Some(4),
            parent: None,
            kind: ChannelKind::Voice,
            tab: FormTab::Permissions,
            name: "Staff".into(),
            topic: String::new(),
            limited: false,
            limit: 5,
            had_password: false,
            protected: false,
            password: String::new(),
            temporary: false,
            focused: false,
            overrides: saved.to_vec(),
            permissions_role: RoleId::EVERYONE,
        };
        assert_eq!(form.override_changes(4, &saved), []);
        assert_eq!(form.changed_settings(&saved), 0);
        let dj = Override::allowing(RoleId(7), Perms::of(&[Perm::Speak]));
        form.overrides = vec![saved[0], saved[2], dj];
        let set = |o: Override| C2S::SetOverride { channel: 4, role: o.role, allow: o.allow(), deny: o.deny() };
        let cleared = Override::inheriting(RoleId::MODERATOR);
        assert_eq!(form.override_changes(4, &saved), [set(cleared), set(dj)]);
        assert_eq!(form.changed_settings(&saved), 2);
    }

    #[test]
    fn deleting_says_what_goes_with_it_and_where_people_go() {
        let names = |n: &[&str]| n.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(delete_note(&[], 0, "Lobby"), "Nobody is in it.");
        assert_eq!(delete_note(&names(&["Duo queue"]), 0, "Lobby"), "Duo queue is deleted with it.");
        assert_eq!(
            delete_note(&names(&["Duo queue"]), 2, "Lobby"),
            "Duo queue is deleted with it. The 2 people inside move to Lobby."
        );
        assert_eq!(
            delete_note(&names(&["A", "B", "C", "D"]), 1, "Lobby"),
            "A, B and 2 more are deleted with it. The person inside moves to Lobby."
        );
    }
}
