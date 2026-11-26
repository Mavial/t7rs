use std::collections::BTreeMap;

use egui::{Event, Key, KeyboardShortcut, Modifiers};
use serde::{Deserialize, Serialize};

use crate::{audio::PttKey, ptt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Main,
    Dialog,
    Form(Action),
    FirstRun,
    Composer,
    Chat,
    Code,
    Viewer,
    Hold,
    Capture,
}

impl Scope {
    fn overlaps(self, other: Self) -> bool {
        if self == other {
            return true;
        }
        match (self, other) {
            (Capture, _) | (_, Capture) => false,
            (Hold, _) | (_, Hold) => true,
            (Viewer, _) | (_, Viewer) => false,
            (Main, _)
            | (_, Main)
            | (Dialog, Form(_))
            | (Form(_), Dialog)
            | (Composer, Chat | Code)
            | (Chat | Code, Composer) => true,
            _ => false,
        }
    }
}

macro_rules! actions {
    ($($action:ident, $label:literal, $scope:expr, $default:expr;)*) => {
        #[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Action { $($action,)* }

        impl Action {
            pub const ALL: &[Self] = &[$(Self::$action,)*];

            pub fn label(self) -> &'static str {
                match self { $(Self::$action => $label,)* }
            }

            pub fn scope(self) -> Scope {
                match self { $(Self::$action => $scope,)* }
            }

            fn default_chord(self) -> Option<Chord> {
                match self { $(Self::$action => $default,)* }
            }
        }
    };
}

use Scope::{Capture, Chat, Code, Composer, Dialog, FirstRun, Form, Hold, Main, Viewer};

actions! {
    OpenConnect, "Open Connect", Main, Some(new_chord(Mods::COMMAND, Key::S));
    ToggleMic, "Toggle microphone mute", Main, Some(new_chord(Mods::COMMAND_SHIFT, Key::M));
    ToggleSound, "Toggle incoming voice", Main, Some(new_chord(Mods::COMMAND_SHIFT, Key::D));
    ToggleAway, "Toggle away", Main, Some(new_chord(Mods::COMMAND_SHIFT, Key::A));
    OpenSettings, "Open Settings", Main, Some(new_chord(Mods::COMMAND, Key::Comma));
    HoldToTalk, "Hold to talk", Hold, None;
    SendChat, "Send message", Chat, ENTER;
    ChatNewline, "Insert line break", Chat, Some(new_chord(Mods::SHIFT, Key::Enter));
    ChatIndent, "Insert tab", Chat, None;
    SendCode, "Send message with an open code block", Code, Some(new_chord(Mods::COMMAND, Key::Enter));
    CodeNewline, "Insert line break in code", Code, ENTER;
    CodeIndent, "Indent code", Code, Some(new_chord(Mods::NONE, Key::Tab));
    CodeOutdent, "Outdent code", Code, Some(new_chord(Mods::SHIFT, Key::Tab));
    ChatPaste, "Paste formatted text or an image", Composer, Some(new_chord(Mods::COMMAND, Key::V));
    DismissDialog, "Dismiss dialog", Dialog, Some(new_chord(Mods::NONE, Key::Escape));
    FirstConnect, "Connect on the welcome screen", FirstRun, ENTER;
    Connect, "Submit Connect dialog", Form(Self::Connect), ENTER;
    CreateChannel, "Create channel", Form(Self::CreateChannel), ENTER;
    CreateSubchannel, "Create sub-channel", Form(Self::CreateSubchannel), ENTER;
    EditChannel, "Save channel changes", Form(Self::EditChannel), ENTER;
    JoinPassword, "Join a password-protected channel", Form(Self::JoinPassword), ENTER;
    ClaimOwnership, "Claim server ownership", Form(Self::ClaimOwnership), ENTER;
    SendPoke, "Send poke", Form(Self::SendPoke), ENTER;
    KickChannel, "Kick from channel", Form(Self::KickChannel), ENTER;
    KickServer, "Kick from server", Form(Self::KickServer), ENTER;
    Ban, "Ban from server", Form(Self::Ban), ENTER;
    CreateRole, "Create role", Form(Self::CreateRole), ENTER;
    RenameRole, "Save role name", Form(Self::RenameRole), ENTER;
    ToggleFullscreen, "Toggle fullscreen", Viewer, Some(new_chord(Mods::NONE, Key::F11));
    ExitFullscreen, "Exit fullscreen", Viewer, Some(new_chord(Mods::NONE, Key::Escape));
    CancelCapture, "Cancel shortcut recording", Capture, Some(new_chord(Mods::NONE, Key::Escape));
}

impl Action {
    pub fn group(self) -> &'static str {
        match self.scope() {
            Main => "T7 window",
            Hold => "Voice",
            Composer | Chat | Code => "Chat composer",
            Dialog | Form(_) | FirstRun => "Dialogs and forms",
            Viewer => "Screens window",
            Capture => "Shortcut recording",
        }
    }
}

const ENTER: Option<Chord> = Some(new_chord(Mods::NONE, Key::Enter));

const fn new_chord(mods: Mods, key: Key) -> Chord {
    Chord { mods, key }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Mods {
    command: bool,
    control: bool,
    alt: bool,
    shift: bool,
}

impl Mods {
    const NONE: Self = Self { command: false, control: false, alt: false, shift: false };
    const COMMAND: Self = Self { command: true, ..Self::NONE };
    const SHIFT: Self = Self { shift: true, ..Self::NONE };
    const COMMAND_SHIFT: Self = Self { command: true, ..Self::SHIFT };

    fn from_egui(m: Modifiers) -> Self {
        Self {
            command: m.command || m.mac_cmd,
            control: cfg!(target_os = "macos") && m.ctrl,
            alt: m.alt,
            shift: m.shift,
        }
    }

    fn egui(self) -> Modifiers {
        let mac = cfg!(target_os = "macos");
        Modifiers {
            command: self.command,
            ctrl: if mac { self.control } else { self.command },
            mac_cmd: mac && self.command,
            alt: self.alt,
            shift: self.shift,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chord {
    #[serde(flatten)]
    mods: Mods,
    #[serde(with = "key_name")]
    pub key: Key,
}

mod key_name {
    use egui::Key;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    #[allow(clippy::trivially_copy_pass_by_ref)] // serde passes the field by reference.
    pub fn serialize<S: Serializer>(key: &Key, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(key.name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Key, D::Error> {
        let name = String::deserialize(d)?;
        Key::from_name(&name).ok_or_else(|| D::Error::custom(format!("unknown key {name}")))
    }
}

impl Chord {
    pub fn from_press(event: &Event) -> Option<(Self, bool)> {
        let Event::Key { key, pressed: true, repeat, modifiers, .. } = *event else { return None };
        use Key::{AltLeft, AltRight, ControlLeft, ControlRight, ShiftLeft, ShiftRight, SuperLeft, SuperRight};
        if matches!(
            key,
            AltLeft | AltRight | ControlLeft | ControlRight | ShiftLeft | ShiftRight | SuperLeft | SuperRight
        ) {
            return None;
        }
        Some((Self { mods: Mods::from_egui(modifiers), key }, repeat))
    }

    fn label(self, ctx: &egui::Context) -> String {
        ctx.format_shortcut(&KeyboardShortcut::new(self.mods.egui(), self.key))
    }

    fn clipboard(self) -> bool {
        use Key::{C, Copy, Cut, Delete, Insert, Paste, V, X};
        matches!(self.key, Cut | Copy | Paste)
            || self.mods.command && !self.mods.alt && !self.mods.shift && matches!(self.key, X | C | V)
            || cfg!(windows)
                && ((self.mods.shift && matches!(self.key, Delete | Insert))
                    || (self.mods.command && self.key == Insert))
    }

    pub fn types_text(self) -> bool {
        use Key::{
            Backslash, Backtick, CloseBracket, CloseCurlyBracket, Colon, Comma, Equals, Exclamationmark, Minus,
            OpenBracket, OpenCurlyBracket, Period, Pipe, Plus, Questionmark, Quote, Semicolon, Slash, Space,
        };
        let m = self.mods;
        !(m.command || m.control || m.alt)
            && (self.key.name().len() == 1
                || matches!(
                    self.key,
                    Space
                        | Colon
                        | Comma
                        | Minus
                        | Period
                        | Plus
                        | Equals
                        | Semicolon
                        | Backslash
                        | Slash
                        | Pipe
                        | Questionmark
                        | Exclamationmark
                        | OpenBracket
                        | CloseBracket
                        | OpenCurlyBracket
                        | CloseCurlyBracket
                        | Backtick
                        | Quote
                ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    Chord(Chord),
    Hold(PttKey),
}

impl Binding {
    pub fn label(self, ctx: &egui::Context) -> String {
        match self {
            Self::Chord(chord) => chord.label(ctx),
            Self::Hold(key) => ptt::label(key.vk),
        }
    }

    fn overlaps(self, other: Self) -> bool {
        match (self, other) {
            (Self::Chord(a), Self::Chord(b)) => a == b,
            (Self::Hold(a), Self::Chord(b)) | (Self::Chord(b), Self::Hold(a)) => {
                ptt::overlaps(a.vk, b.key, b.mods.egui())
            }
            (Self::Hold(a), Self::Hold(b)) => a == b,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Override {
    Key(Chord),
    Unbound,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Bindings(BTreeMap<Action, Override>);

impl Bindings {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn chord(&self, action: Action) -> Option<Chord> {
        match self.0.get(&action) {
            Some(Override::Key(chord)) => Some(*chord),
            Some(Override::Unbound) => None,
            None => action.default_chord(),
        }
    }

    pub fn get(&self, action: Action, hold: Option<PttKey>) -> Option<Binding> {
        if action == Action::HoldToTalk { hold.map(Binding::Hold) } else { self.chord(action).map(Binding::Chord) }
    }

    pub fn is_default(&self, action: Action, hold: Option<PttKey>) -> bool {
        if action == Action::HoldToTalk { hold.is_none() } else { !self.0.contains_key(&action) }
    }

    pub fn hint(&self, ctx: &egui::Context, action: Action, label: &str) -> String {
        self.chord(action).map_or_else(|| label.to_owned(), |key| format!("{label} ({})", key.label(ctx)))
    }

    pub fn set(&mut self, action: Action, hold: &mut Option<PttKey>, binding: Option<Binding>) -> Result<(), String> {
        if let Some(binding) = binding {
            if (action == Action::HoldToTalk) != matches!(binding, Binding::Hold(_)) {
                return Err("This action needs a different kind of binding.".into());
            }
            if self.get(action, *hold) == Some(binding) {
                return Ok(());
            }
            if let Binding::Chord(key) = binding {
                let default_chat_paste = action == Action::ChatPaste && Some(key) == action.default_chord();
                if action.scope() != Viewer && key.clipboard() && !default_chat_paste {
                    return Err("This shortcut is reserved for clipboard editing.".into());
                }
            }
            if let Some(other) = Action::ALL.iter().copied().find(|&other| {
                action != other
                    && action.scope().overlaps(other.scope())
                    && self.get(other, *hold).is_some_and(|existing| binding.overlaps(existing))
            }) {
                return Err(format!("Already used by {}.", other.label()));
            }
        }

        match binding {
            Some(Binding::Hold(key)) => *hold = Some(key),
            None if action == Action::HoldToTalk => *hold = None,
            Some(Binding::Chord(key)) if Some(key) != action.default_chord() => {
                self.0.insert(action, Override::Key(key));
            }
            None if action.default_chord().is_some() => {
                self.0.insert(action, Override::Unbound);
            }
            _ => {
                self.0.remove(&action);
            }
        }
        Ok(())
    }

    pub fn reset(&mut self, action: Action, hold: &mut Option<PttKey>) -> Result<(), String> {
        self.set(action, hold, action.default_chord().map(Binding::Chord))
    }

    pub fn uses_platform_paste(&self) -> bool {
        self.chord(Action::ChatPaste) == Action::ChatPaste.default_chord()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_conflict_only_where_they_can_fire_together() {
        let mut bindings = Bindings::default();
        let mut hold = None;
        let f6 = Some(Binding::Chord(new_chord(Mods::NONE, Key::F6)));
        bindings.set(Action::SendChat, &mut hold, f6).unwrap();
        bindings.set(Action::SendCode, &mut hold, f6).unwrap();
        bindings.set(Action::ToggleFullscreen, &mut hold, f6).unwrap();
        assert!(bindings.set(Action::ChatPaste, &mut hold, f6).is_err());
        assert!(bindings.set(Action::ToggleMic, &mut hold, f6).is_err());
        assert!(bindings.set(Action::HoldToTalk, &mut hold, Some(Binding::Hold(PttKey { vk: 0x75 }))).is_err());
        assert_eq!(bindings.chord(Action::ChatPaste).unwrap().key, Key::V);
        assert!(hold.is_none());

        bindings.set(Action::Connect, &mut hold, f6).unwrap();
        bindings.set(Action::FirstConnect, &mut hold, f6).unwrap();
        assert!(bindings.set(Action::DismissDialog, &mut hold, f6).is_err());
        bindings.reset(Action::SendChat, &mut hold).unwrap();
        assert_eq!(bindings.chord(Action::SendChat).unwrap().key, Key::Enter);
    }

    #[test]
    fn modified_shortcuts_cannot_open_push_to_talk() {
        let modifiers = Mods { alt: true, shift: true, command: true, control: cfg!(target_os = "macos") };
        let shortcut = Some(Binding::Chord(new_chord(modifiers, Key::F6)));
        let mut bindings = Bindings::default();
        for vk in [0x75, 0x11, 0x10, 0x12] {
            // F6, Ctrl, Shift, Alt
            let mut hold = Some(PttKey { vk });
            assert!(bindings.set(Action::ToggleAway, &mut hold, shortcut).is_err(), "held key {vk:#04x}");
        }

        let mut hold = None;
        bindings.set(Action::ToggleAway, &mut hold, shortcut).unwrap();
        for vk in [0x75, 0x12] {
            assert!(bindings.set(Action::HoldToTalk, &mut hold, Some(Binding::Hold(PttKey { vk }))).is_err());
            assert!(hold.is_none());
        }
        bindings.set(Action::HoldToTalk, &mut hold, Some(Binding::Hold(PttKey { vk: 0x76 }))).unwrap();
    }

    #[test]
    fn clipboard_chords_remain_available_to_standard_text_editing() {
        let mut bindings = Bindings::default();
        let mut hold = None;
        let copy = Some(Binding::Chord(new_chord(Mods::COMMAND, Key::C)));
        assert!(bindings.set(Action::SendChat, &mut hold, copy).is_err());
        bindings.set(Action::ToggleFullscreen, &mut hold, copy).unwrap();
        #[cfg(windows)]
        assert!(
            bindings
                .set(Action::SendChat, &mut hold, Some(Binding::Chord(new_chord(Mods::SHIFT, Key::Insert))))
                .is_err()
        );
    }
}
