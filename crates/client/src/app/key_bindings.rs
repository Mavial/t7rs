use egui::{Align, Event, Key, Layout, Modifiers, RawInput};

use super::widgets::{Item, hint, menu_button, menu_style, section};
use super::{App, Conn};
use crate::audio::PttKey;
use crate::bindings::{Action, BackgroundShortcuts, Binding, Bindings, Chord, Scope};
use crate::hotkey::{Hotkeys, State};
use crate::icons::Icon;
use crate::theme::Btn;
use crate::{icons, ptt, theme};

pub(super) struct Capture {
    action: Action,
    pending: Option<(Binding, Option<Key>)>,
    error: Option<String>,
    modifiers: Modifiers,
}

fn key_identity(event: &Event) -> Option<(Key, bool)> {
    let Event::Key { key, physical_key, pressed, .. } = *event else { return None };
    Some((physical_key.unwrap_or(key), pressed))
}

impl Capture {
    fn event(&mut self, event: &Event) -> Option<Binding> {
        if matches!(event, Event::WindowFocused(false)) {
            self.pending = None;
            self.modifiers = Modifiers::NONE;
            return None;
        }
        let previous_modifiers = self.modifiers;
        if let Event::ModifiersChanged(modifiers) = *event {
            self.modifiers = modifiers;
        }
        let identity = key_identity(event);
        if let Some((binding, physical_key)) = self.pending {
            let released = match (physical_key, binding) {
                (Some(key), _) => identity == Some((key, false)),
                (None, Binding::Hold(key)) => ptt::transition(key.vk, event) == Some(false),
                (None, Binding::Chord(_)) => false,
            };
            if released {
                self.pending = None;
                return Some(binding);
            }
            return None;
        }

        let binding = if self.action == Action::HoldToTalk {
            Binding::Hold(PttKey { vk: ptt::capture(event, previous_modifiers)? })
        } else {
            let (chord, false) = Chord::from_press(event)? else { return None };
            Binding::Chord(chord)
        };
        self.pending = Some((binding, identity.map(|(key, _)| key)));
        self.error = None;
        None
    }
}

#[derive(Default)]
struct Context {
    form: Option<Action>,
    dismiss: bool,
    composer: bool,
    send_button: bool,
    code: bool,
}

impl Context {
    fn accepts(&self, action: Action) -> bool {
        if self.send_button && matches!(action, Action::SendChat | Action::SendCode) {
            return self.code == (action == Action::SendCode);
        }
        match action.scope() {
            Scope::Main => true,
            Scope::Dialog => self.dismiss,
            Scope::FirstRun | Scope::Form(_) => self.form == Some(action),
            Scope::Composer => self.composer,
            Scope::Chat => self.composer && !self.code,
            Scope::Code => self.composer && self.code,
            Scope::Viewer | Scope::Hold | Scope::Capture => false,
        }
    }
}

pub(super) const COMPOSER: &str = "chat-composer";
pub(super) const SEND_BUTTON: &str = "chat-send";

impl App {
    pub(crate) fn capturing_binding(&self) -> bool {
        self.settings_dialog.as_ref().is_some_and(|dlg| dlg.capture.is_some())
    }

    pub(crate) fn clipboard_shortcuts(&self) -> bool {
        !self.capturing_binding() && self.captured_keys.is_empty()
    }

    fn key_context(&self, ctx: &egui::Context) -> Context {
        let top = ctx.memory(egui::Memory::top_modal_layer).map(|layer| layer.id);
        if let Some(top) = top {
            let is = |name| top == egui::Id::unique(name);
            let form = if is("connect") && self.form.is_some() && !self.first_run() {
                Some(Action::Connect)
            } else if is("poke") && self.poke_form.is_some() {
                Some(Action::SendPoke)
            } else if is("moderation") {
                self.moderation.as_ref().map(super::Moderation::submit_action)
            } else if is("channel-dialog") {
                self.channel_dialog.as_ref().and_then(super::ChannelDialog::submit_action)
            } else if is("server-settings") {
                self.server_settings.as_ref().and_then(|s| s.role_submit_action(ctx))
            } else {
                None
            };
            return Context { form, dismiss: !egui::Popup::is_any_open(ctx), ..Default::default() };
        }

        let id = egui::Id::unique(COMPOSER);
        let focus = ctx.memory(egui::Memory::focused);
        let connected = matches!(self.conn, Conn::Connected { .. });
        let composer = connected && focus == Some(id);
        let send_button = connected && focus == Some(egui::Id::unique(SEND_BUTTON));
        let cursor = egui::TextEdit::load_state(ctx, id)
            .and_then(|s| s.cursor.char_range())
            .map_or(usize::MAX, |r| r.primary.index.into());
        Context {
            form: self.first_run().then_some(Action::FirstConnect),
            composer,
            send_button,
            code: (composer || send_button) && super::markdown::in_fenced_code(&self.input, cursor),
            ..Default::default()
        }
    }

    pub(crate) fn prepare_input(
        &mut self,
        ctx: &egui::Context,
        input: &mut RawInput,
        mut clipboard: impl FnMut() -> Option<Event>,
    ) {
        self.pressed.clear();
        let hold = self.settings.audio.ptt_key;
        if !input.focused {
            self.local_ptt = ptt::Held::Up;
            self.captured_keys.clear();
        }
        if let Some(hold) = hold {
            for event in &input.events {
                self.local_ptt.update(hold.vk, event);
            }
        }
        let capturing = self.capturing_binding();
        self.voice.shared.set_ptt_local(hold.is_some() && self.local_ptt.is_down() && !capturing);
        if capturing {
            self.capture_input(&mut input.events);
            return;
        }
        if input.events.is_empty() {
            return;
        }

        let context = self.key_context(ctx);
        let editing_text =
            ctx.memory(egui::Memory::focused).is_some_and(|id| egui::TextEdit::load_state(ctx, id).is_some());
        let mut suppress_text = false;
        let platform_paste = self.settings.bindings.uses_platform_paste();
        input.events = std::mem::take(&mut input.events)
            .into_iter()
            .filter_map(|event| {
                if std::mem::take(&mut suppress_text) && matches!(event, Event::Text(_)) {
                    return None;
                }
                if let Some((key, down)) = key_identity(&event)
                    && self.captured_keys.contains(&key)
                {
                    if !down {
                        self.captured_keys.remove(&key);
                    }
                    suppress_text = down;
                    return None;
                }
                if let Some((chord, repeat)) = Chord::from_press(&event) {
                    let action = Action::ALL.iter().copied().find(|&a| {
                        context.accepts(a)
                            && self.settings.bindings.chord(a) == Some(chord)
                            && !(a.scope() == Scope::Main && editing_text && chord.types_text())
                    });
                    if let Some(action) = action {
                        suppress_text = true;
                        return match action {
                            Action::ChatNewline | Action::CodeNewline => Some(Event::Paste("\n".into())),
                            Action::ChatIndent | Action::CodeIndent => Some(Event::Paste("\t".into())),
                            Action::ChatPaste if !repeat => clipboard().map(|event| self.chat_paste(ctx, event)),
                            _ => {
                                if !repeat {
                                    self.pressed.insert(action);
                                }
                                None
                            }
                        };
                    }
                    // egui also sends a click to a focused button on Enter.
                    if chord.key == Key::Enter && (context.form.is_some() || context.composer || context.send_button) {
                        return None;
                    }
                }
                match event {
                    Event::Paste(_) | Event::PasteImage(_) if context.composer && platform_paste => {
                        Some(self.chat_paste(ctx, event))
                    }
                    Event::PasteImage(_) if context.composer => None,
                    _ => Some(event),
                }
            })
            .collect();
    }

    fn chat_paste(&self, ctx: &egui::Context, event: Event) -> Event {
        if let Event::Paste(text) = &event {
            let range = egui::TextEdit::load_state(ctx, egui::Id::unique(COMPOSER)).and_then(|s| s.cursor.char_range());
            let inside_code =
                range.is_some_and(|r| super::markdown::in_code(&self.input, r.as_sorted_char_range().start.into()));
            if !inside_code && let Some(markdown) = super::markdown::rich_paste(text) {
                return Event::Paste(markdown);
            }
        }
        event
    }

    fn capture_input(&mut self, events: &mut Vec<Event>) {
        let dialog = self.settings_dialog.as_mut().expect("capturing in settings");
        let cancel = self.settings.bindings.chord(Action::CancelCapture);
        events.retain(|event| {
            if matches!(event, Event::WindowFocused(false)) {
                self.captured_keys.clear();
            }
            if let Some((key, down)) = key_identity(event) {
                if down {
                    self.captured_keys.insert(key);
                } else {
                    self.captured_keys.remove(&key);
                }
            }
            if Chord::from_press(event).is_some_and(|(chord, repeat)| !repeat && Some(chord) == cancel) {
                dialog.capture = None;
            } else if let Some(capture) = &mut dialog.capture
                && let Some(binding) = capture.event(event)
            {
                match self.settings.bindings.set(capture.action, &mut self.settings.audio.ptt_key, Some(binding)) {
                    Ok(()) => dialog.capture = None,
                    Err(error) => capture.error = Some(error),
                }
            }
            !matches!(
                event,
                Event::Key { .. }
                    | Event::Text(_)
                    | Event::Paste(_)
                    | Event::PasteImage(_)
                    | Event::Copy
                    | Event::Cut
                    | Event::Ime(_)
            )
        });
        if !self.capturing_binding() {
            self.local_ptt = ptt::Held::Up;
            self.voice.shared.set_settings(&self.settings.audio);
            self.voice.shared.finish_binding_capture();
        }
    }
}

pub(super) fn modal_closed<T>(modal: &egui::ModalResponse<T>, dismiss: bool) -> bool {
    modal.backdrop_response.clicked() || modal.response.should_close() || (modal.is_top_modal && dismiss)
}

pub(super) fn bindings_page(
    ui: &mut egui::Ui,
    bindings: &mut Bindings,
    hold: &mut Option<PttKey>,
    background: &mut BackgroundShortcuts,
    hotkeys: &Hotkeys,
    capture: &mut Option<Capture>,
    error: &mut Option<(Action, String)>,
) {
    const VOICE: [Action; 3] = [Action::ToggleMic, Action::ToggleSound, Action::HoldToTalk];
    let other = Action::ALL.iter().copied().filter(|action| !VOICE.contains(action));
    ui.spacing_mut().item_spacing.y = 4.0;
    let mut group = "";
    for action in VOICE.into_iter().chain(other) {
        let next_group = if VOICE.contains(&action) { "Voice" } else { action.group() };
        if next_group != group {
            group = next_group;
            section(ui, group);
        }
        ui.push_id(action, |ui| {
            let binding = bindings.get(action, *hold);
            let label = binding.map_or_else(|| "Unbound".into(), |key| key.label(ui.ctx()));
            let mutating = capture.as_ref().is_some_and(|c| c.action == action);
            let idle = capture.is_none();
            ui.horizontal(|ui| {
                ui.label(action.label());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let more = ui.add_enabled_ui(idle, |ui| menu_button(ui, Icon::Dots)).inner;
                    let more = more.on_hover_text("Shortcut options");
                    if idle {
                        egui::Popup::menu(&more).align(egui::RectAlign::BOTTOM_END).show(|ui| {
                            menu_style(ui, 142.0);
                            if ui.add_enabled(binding.is_some(), Item::new(Icon::Cross, "Clear shortcut")).clicked() {
                                *error = bindings.set(action, hold, None).err().map(|error| (action, error));
                            }
                            if ui
                                .add_enabled(
                                    !bindings.is_default(action, *hold),
                                    Item::new(Icon::Update, "Restore default"),
                                )
                                .clicked()
                            {
                                *error = bindings.reset(action, hold).err().map(|error| (action, error));
                            }
                        });
                    }
                    const KEY_WIDTH: f32 = 122.0;
                    if mutating {
                        let tip = bindings.hint(ui.ctx(), Action::CancelCapture, "Cancel recording");
                        if ui.add(Btn::secondary("Cancel").small().min_width(KEY_WIDTH)).on_hover_text(tip).clicked() {
                            *capture = None;
                        }
                    } else if ui.add_enabled(idle, Btn::secondary(&label).small().min_width(KEY_WIDTH)).clicked() {
                        *error = None;
                        *capture =
                            Some(Capture { action, pending: None, error: None, modifiers: ui.input(|i| i.modifiers) });
                    }
                    if cfg!(windows) && BackgroundShortcuts::ACTIONS.contains(&action) {
                        ui.add_enabled_ui(idle, |ui| background_toggle(ui, action, background, hotkeys.state(action)));
                    }
                });
            });
            if let Some(capture) = capture.as_ref().filter(|c| c.action == action) {
                if let Some(error) = &capture.error {
                    ui.colored_label(theme::RED, error);
                } else if let Some((binding, _)) = capture.pending {
                    hint(ui, &format!("Release {} to assign it.", binding.label(ui.ctx())));
                } else if action == Action::HoldToTalk {
                    hint(ui, "Press a key or a middle or side mouse button.");
                } else {
                    hint(ui, "Press the shortcut you want to use.");
                }
            } else if let Some((_, error)) = error.as_ref().filter(|(row, _)| *row == action) {
                ui.colored_label(theme::RED, error);
            }
            if cfg!(windows)
                && idle
                && background.get(action)
                && let State::Failed(why) = hotkeys.state(action)
            {
                ui.colored_label(theme::RED, format!("Can't use in the background because {why}."));
            }
        });
    }
}

fn background_toggle(ui: &mut egui::Ui, action: Action, background: &mut BackgroundShortcuts, state: &State) {
    let (rect, mut response) = ui.allocate_exact_size(egui::Vec2::splat(28.0), egui::Sense::click());
    if response.clicked() {
        background.set(action, !background.get(action));
        response.mark_changed();
    }
    let on = background.get(action);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::CheckBox, response.enabled(), on, "Works in background")
    });
    let (fill, color) = match (on, state) {
        (true, State::Failed(_)) => (theme::RED.gamma_multiply(0.14), theme::RED),
        (true, _) => (theme::ACCENT_BG, theme::ACCENT_TEXT),
        (false, _) if response.is_pointer_button_down_on() => (theme::SURFACE_ACTIVE, theme::TEXT),
        (false, _) if response.hovered() => (theme::SURFACE_HOVER, theme::TEXT),
        (false, _) => (egui::Color32::TRANSPARENT, theme::TEXT_DIM),
    };
    let focus = if response.has_focus() { egui::Stroke::new(1.0, theme::ACCENT_TEXT) } else { egui::Stroke::NONE };
    let painter = ui.painter();
    painter.rect(rect, theme::RADIUS, fill, focus, egui::StrokeKind::Inside);
    icons::paint(painter, egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(18.0)), Icon::Globe, color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(if on {
        "Use this shortcut only in T7"
    } else {
        "Use this shortcut when T7 is in the background"
    });
}
