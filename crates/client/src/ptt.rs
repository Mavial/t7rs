//! Push-to-talk keys. Keys are stored as Windows virtual-key codes, which the
//! capture thread can poll globally (while a game has focus). This module maps
//! them to and from egui input for binding, labels and the focused fallback.

use egui::{InputState, Key, PointerButton};

const CONTROL: u16 = 0x11;
const SHIFT: u16 = 0x10;
const ALT: u16 = 0x12;

/// (virtual-key code, egui key, label)
const KEYS: &[(u16, Key, &str)] = &[
    (0x20, Key::Space, "Space"),
    (0x09, Key::Tab, "Tab"),
    (0x08, Key::Backspace, "Backspace"),
    (0x0D, Key::Enter, "Enter"),
    (0x2D, Key::Insert, "Insert"),
    (0x2E, Key::Delete, "Delete"),
    (0x24, Key::Home, "Home"),
    (0x23, Key::End, "End"),
    (0x21, Key::PageUp, "Page Up"),
    (0x22, Key::PageDown, "Page Down"),
    (0x25, Key::ArrowLeft, "Left"),
    (0x26, Key::ArrowUp, "Up"),
    (0x27, Key::ArrowRight, "Right"),
    (0x28, Key::ArrowDown, "Down"),
    (0xC0, Key::Backtick, "`"),
    (0xBD, Key::Minus, "-"),
    (0xBB, Key::Equals, "="),
    (0xDB, Key::OpenBracket, "["),
    (0xDD, Key::CloseBracket, "]"),
    (0xDC, Key::Backslash, "\\"),
    (0xBA, Key::Semicolon, ";"),
    (0xDE, Key::Quote, "'"),
    (0xBC, Key::Comma, ","),
    (0xBE, Key::Period, "."),
    (0xBF, Key::Slash, "/"),
    (0x30, Key::Num0, "0"),
    (0x31, Key::Num1, "1"),
    (0x32, Key::Num2, "2"),
    (0x33, Key::Num3, "3"),
    (0x34, Key::Num4, "4"),
    (0x35, Key::Num5, "5"),
    (0x36, Key::Num6, "6"),
    (0x37, Key::Num7, "7"),
    (0x38, Key::Num8, "8"),
    (0x39, Key::Num9, "9"),
    (0x41, Key::A, "A"),
    (0x42, Key::B, "B"),
    (0x43, Key::C, "C"),
    (0x44, Key::D, "D"),
    (0x45, Key::E, "E"),
    (0x46, Key::F, "F"),
    (0x47, Key::G, "G"),
    (0x48, Key::H, "H"),
    (0x49, Key::I, "I"),
    (0x4A, Key::J, "J"),
    (0x4B, Key::K, "K"),
    (0x4C, Key::L, "L"),
    (0x4D, Key::M, "M"),
    (0x4E, Key::N, "N"),
    (0x4F, Key::O, "O"),
    (0x50, Key::P, "P"),
    (0x51, Key::Q, "Q"),
    (0x52, Key::R, "R"),
    (0x53, Key::S, "S"),
    (0x54, Key::T, "T"),
    (0x55, Key::U, "U"),
    (0x56, Key::V, "V"),
    (0x57, Key::W, "W"),
    (0x58, Key::X, "X"),
    (0x59, Key::Y, "Y"),
    (0x5A, Key::Z, "Z"),
    (0x70, Key::F1, "F1"),
    (0x71, Key::F2, "F2"),
    (0x72, Key::F3, "F3"),
    (0x73, Key::F4, "F4"),
    (0x74, Key::F5, "F5"),
    (0x75, Key::F6, "F6"),
    (0x76, Key::F7, "F7"),
    (0x77, Key::F8, "F8"),
    (0x78, Key::F9, "F9"),
    (0x79, Key::F10, "F10"),
    (0x7A, Key::F11, "F11"),
    (0x7B, Key::F12, "F12"),
];

/// (virtual-key code, mouse button, label)
const BUTTONS: &[(u16, PointerButton, &str)] = &[
    (0x04, PointerButton::Middle, "Middle mouse"),
    (0x05, PointerButton::Extra1, "Mouse 4"),
    (0x06, PointerButton::Extra2, "Mouse 5"),
];

/// The key or button pressed this frame, for binding. Escape is left to cancel.
pub fn capture(input: &InputState) -> Option<u16> {
    for ev in &input.events {
        match ev {
            egui::Event::Key { key, pressed: true, .. } => {
                if let Some(&(vk, ..)) = KEYS.iter().find(|(_, k, _)| k == key) {
                    return Some(vk);
                }
            }
            egui::Event::PointerButton { button, pressed: true, .. } => {
                if let Some(&(vk, ..)) = BUTTONS.iter().find(|(_, b, _)| b == button) {
                    return Some(vk);
                }
            }
            _ => {}
        }
    }
    // Modifier keys on their own arrive only as modifier state.
    let m = input.modifiers;
    [(m.ctrl, CONTROL), (m.shift, SHIFT), (m.alt, ALT)].into_iter().find(|(on, _)| *on).map(|(_, vk)| vk)
}

pub fn label(vk: u16) -> String {
    let known = KEYS.iter().map(|&(v, _, l)| (v, l)).chain(BUTTONS.iter().map(|&(v, _, l)| (v, l)));
    let named = [(CONTROL, "Ctrl"), (SHIFT, "Shift"), (ALT, "Alt")];
    known.chain(named).find(|&(v, _)| v == vk).map_or_else(|| format!("Key 0x{vk:02X}"), |(_, l)| l.to_owned())
}

/// Whether the key is held, as far as our own window can tell.
pub fn is_down(vk: u16, input: &InputState) -> bool {
    if let Some(&(_, key, _)) = KEYS.iter().find(|(v, ..)| *v == vk) {
        return input.key_down(key);
    }
    if let Some(&(_, button, _)) = BUTTONS.iter().find(|(v, ..)| *v == vk) {
        return input.pointer.button_down(button);
    }
    match vk {
        CONTROL => input.modifiers.ctrl,
        SHIFT => input.modifiers.shift,
        ALT => input.modifiers.alt,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_labelled() {
        let mut codes: Vec<u16> = KEYS.iter().map(|k| k.0).chain(BUTTONS.iter().map(|b| b.0)).collect();
        codes.extend([CONTROL, SHIFT, ALT]);
        let n = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), n);
        assert_eq!(label(0x56), "V");
        assert_eq!(label(0x05), "Mouse 4");
        assert_eq!(label(CONTROL), "Ctrl");
    }
}
