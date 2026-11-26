//! Windows delivers a registered chord as `WM_HOTKEY` instead of a focused key press.

use crate::bindings::{Action, BackgroundShortcuts, Chord, Hotkey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub chord: Chord,
    pub hotkey: Hotkey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    Off,
    Recording,
    Refused(String),
    Register(Target),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Off,
    Recording,
    Active,
    Failed(String),
}

pub trait Backend {
    fn register(&mut self, id: i32, hotkey: Hotkey) -> Result<(), String>;
    fn unregister(&mut self, id: i32);
}

struct Slot {
    state: State,
    attempted: Option<Target>,
    id: Option<i32>,
}

pub struct Hotkeys<B: Backend = Box<dyn Backend>> {
    backend: B,
    slots: [Slot; BackgroundShortcuts::ACTIONS.len()],
    next_id: i32,
}

impl Hotkeys {
    pub fn new() -> Self {
        Self::with_backend(Box::new(Os))
    }
}

impl Backend for Box<dyn Backend> {
    fn register(&mut self, id: i32, hotkey: Hotkey) -> Result<(), String> {
        (**self).register(id, hotkey)
    }

    fn unregister(&mut self, id: i32) {
        (**self).unregister(id);
    }
}

impl<B: Backend> Hotkeys<B> {
    pub fn with_backend(backend: B) -> Self {
        let slots = std::array::from_fn(|_| Slot { state: State::Off, attempted: None, id: None });
        Self { backend, slots, next_id: 0 }
    }

    fn slot(&self, action: Action) -> Option<&Slot> {
        BackgroundShortcuts::ACTIONS.iter().position(|&a| a == action).map(|i| &self.slots[i])
    }

    pub fn state(&self, action: Action) -> &State {
        self.slot(action).map_or(&State::Off, |slot| &slot.state)
    }

    #[cfg(any(windows, test))]
    pub fn resolve(&self, id: i32) -> Option<(Action, Chord)> {
        BackgroundShortcuts::ACTIONS.iter().zip(&self.slots).find_map(|(&action, slot)| {
            match (slot.id, slot.attempted) {
                (Some(live), Some(target)) if live == id => Some((action, target.chord)),
                _ => None,
            }
        })
    }

    pub fn retry_failed(&mut self) {
        for slot in &mut self.slots {
            if matches!(slot.state, State::Failed(_)) {
                slot.attempted = None;
            }
        }
    }

    pub fn sync(&mut self, want: impl Fn(Action) -> Want) -> bool {
        let mut changed = false;
        for (slot, action) in self.slots.iter_mut().zip(BackgroundShortcuts::ACTIONS) {
            let before = slot.state.clone();
            match want(action) {
                Want::Register(target) if slot.attempted == Some(target) => {}
                Want::Register(target) => {
                    release(&mut self.backend, slot);
                    self.next_id = self.next_id % MAX_ID + 1;
                    slot.attempted = Some(target);
                    match self.backend.register(self.next_id, target.hotkey) {
                        Ok(()) => {
                            slot.id = Some(self.next_id);
                            slot.state = State::Active;
                        }
                        Err(why) => slot.state = State::Failed(why),
                    }
                }
                Want::Off => {
                    release(&mut self.backend, slot);
                    slot.state = State::Off;
                }
                Want::Recording => {
                    release(&mut self.backend, slot);
                    slot.state = State::Recording;
                }
                Want::Refused(why) => {
                    release(&mut self.backend, slot);
                    slot.state = State::Failed(why);
                }
            }
            changed |= slot.state != before;
        }
        changed
    }
}

/// Reserve two native IDs per shortcut within Windows' application ID range.
const MAX_ID: i32 = 0xBFFF / 2;

fn release(backend: &mut impl Backend, slot: &mut Slot) {
    if let Some(id) = slot.id.take() {
        backend.unregister(id);
    }
    slot.attempted = None;
}

impl<B: Backend> Drop for Hotkeys<B> {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            release(&mut self.backend, slot);
        }
    }
}

#[cfg(windows)]
pub use windows_impl::{Os, message_id};

#[cfg(windows)]
mod windows_impl {
    use std::ffi::c_void;

    use windows::Win32::Foundation::ERROR_HOTKEY_ALREADY_REGISTERED;
    use windows::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey, VK_NUMPAD0,
    };

    use super::{Backend, MAX_ID};
    use crate::bindings::Hotkey;

    /// `RegisterHotKey` for the calling thread, which must be the one running the event loop.
    pub struct Os;

    const ALREADY_REGISTERED: i32 = ERROR_HOTKEY_ALREADY_REGISTERED.0.cast_signed();

    fn native_ids(id: i32) -> [i32; 2] {
        [id * 2, id * 2 + 1]
    }

    fn registrations(id: i32, vk: u16) -> impl Iterator<Item = (i32, u32)> {
        let [primary_id, keypad_id] = native_ids(id);
        let primary = u32::from(vk);
        let zero = u32::from(b'0');
        let keypad =
            (zero..=u32::from(b'9')).contains(&primary).then(|| (keypad_id, u32::from(VK_NUMPAD0) + primary - zero));
        std::iter::once((primary_id, primary)).chain(keypad)
    }

    impl Backend for Os {
        fn register(&mut self, id: i32, hotkey: Hotkey) -> Result<(), String> {
            let flags = [(hotkey.ctrl, MOD_CONTROL), (hotkey.alt, MOD_ALT), (hotkey.shift, MOD_SHIFT)]
                .into_iter()
                .filter(|&(on, _)| on)
                .fold(MOD_NOREPEAT, |flags, (_, flag)| flags | flag);
            for (index, (native_id, vk)) in registrations(id, hotkey.vk).enumerate() {
                // SAFETY: a null window registers for the calling thread's message queue.
                if unsafe { RegisterHotKey(std::ptr::null_mut(), native_id, flags, vk) } == 0 {
                    let error = std::io::Error::last_os_error();
                    if index != 0 {
                        self.unregister(id);
                    }
                    return Err(match error.raw_os_error() {
                        Some(ALREADY_REGISTERED) => "another app already uses it".to_owned(),
                        Some(code) => format!("Windows refused it (error {code})"),
                        None => "Windows refused it".to_owned(),
                    });
                }
            }
            Ok(())
        }

        fn unregister(&mut self, id: i32) {
            for native_id in native_ids(id) {
                // SAFETY: a null window addresses the current thread's registrations.
                unsafe { UnregisterHotKey(std::ptr::null_mut(), native_id) };
            }
        }
    }

    /// # Safety
    /// `msg` must point to the live `MSG` from the event loop's message hook.
    pub unsafe fn message_id(msg: *const c_void) -> Option<i32> {
        let msg = unsafe { msg.cast::<MSG>().as_ref() }?;
        if msg.message != WM_HOTKEY || !msg.hwnd.0.is_null() {
            return None;
        }
        let id = i32::try_from(msg.wParam.0 / 2).ok()?;
        (1..=MAX_ID).contains(&id).then_some(id)
    }

    #[test]
    fn digit_registrations_have_distinct_native_ids_and_dispatch_to_one_shortcut() {
        use windows::Win32::Foundation::WPARAM;

        let ids: Vec<_> = registrations(MAX_ID, u16::from(b'7')).collect();
        assert_eq!(ids, [(0xBFFE, 0x37), (0xBFFF, 0x67)]);
        for (native_id, _) in ids {
            let msg = MSG { message: WM_HOTKEY, wParam: WPARAM(native_id as usize), ..MSG::default() };
            // SAFETY: msg is live for the duration of the call.
            assert_eq!(unsafe { message_id(std::ptr::from_ref(&msg).cast()) }, Some(MAX_ID));
        }
    }
}

#[cfg(not(windows))]
pub struct Os;

#[cfg(not(windows))]
impl Backend for Os {
    fn register(&mut self, _: i32, _: Hotkey) -> Result<(), String> {
        Err("background shortcuts need Windows".to_owned())
    }

    fn unregister(&mut self, _: i32) {}
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::rc::Rc;

    use egui::Key;

    use super::*;

    #[derive(Clone, Default)]
    struct Fake {
        live: Rc<RefCell<BTreeSet<i32>>>,
        refuse: Rc<RefCell<bool>>,
    }

    impl Backend for Fake {
        fn register(&mut self, id: i32, _: Hotkey) -> Result<(), String> {
            if *self.refuse.borrow() {
                return Err("another app already uses it".into());
            }
            self.live.borrow_mut().insert(id);
            Ok(())
        }

        fn unregister(&mut self, id: i32) {
            self.live.borrow_mut().remove(&id);
        }
    }

    fn target(key: Key) -> Target {
        let event = egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        };
        let chord = Chord::from_press(&event).unwrap().0;
        Target { chord, hotkey: chord.hotkey().unwrap() }
    }

    fn sync(hotkeys: &mut Hotkeys<Fake>, mic: Want, sound: Want) -> bool {
        hotkeys.sync(|action| if action == Action::ToggleMic { mic.clone() } else { sound.clone() })
    }

    #[test]
    fn a_registration_is_kept_until_something_about_it_changes() {
        let fake = Fake::default();
        let mut hotkeys = Hotkeys::with_backend(fake.clone());
        let (m, d) = (target(Key::M), target(Key::D));
        assert!(sync(&mut hotkeys, Want::Register(m), Want::Off));
        let id = *fake.live.borrow().first().unwrap();
        assert_eq!(hotkeys.resolve(id), Some((Action::ToggleMic, m.chord)));

        assert!(sync(&mut hotkeys, Want::Register(m), Want::Register(d)));
        assert!(!sync(&mut hotkeys, Want::Register(m), Want::Register(d)));
        assert_eq!(fake.live.borrow().len(), 2);
        assert_eq!(hotkeys.resolve(id), Some((Action::ToggleMic, m.chord)));
        assert_eq!(hotkeys.state(Action::ToggleMic), &State::Active);

        sync(&mut hotkeys, Want::Off, Want::Register(d));
        assert_eq!(fake.live.borrow().len(), 1);
        assert_eq!(hotkeys.state(Action::ToggleMic), &State::Off);
        assert_eq!(hotkeys.resolve(id), None);
    }

    #[test]
    fn recording_a_shortcut_releases_everything_and_drops_late_messages() {
        let fake = Fake::default();
        let mut hotkeys = Hotkeys::with_backend(fake.clone());
        let (m, d) = (target(Key::M), target(Key::D));
        sync(&mut hotkeys, Want::Register(m), Want::Register(d));
        let stale: Vec<i32> = fake.live.borrow().iter().copied().collect();

        hotkeys.sync(|_| Want::Recording);
        assert!(fake.live.borrow().is_empty());
        assert_eq!(hotkeys.state(Action::ToggleSound), &State::Recording);

        sync(&mut hotkeys, Want::Register(m), Want::Register(d));
        assert!(stale.iter().all(|&id| hotkeys.resolve(id).is_none()), "ids are not reused");
        let live: Vec<i32> = fake.live.borrow().iter().copied().collect();
        assert!(live.iter().all(|&id| hotkeys.resolve(id).is_some()));
    }

    #[test]
    fn a_refused_registration_reports_why_and_is_not_retried_until_asked() {
        let fake = Fake::default();
        let mut hotkeys = Hotkeys::with_backend(fake.clone());
        let m = target(Key::M);
        *fake.refuse.borrow_mut() = true;
        sync(&mut hotkeys, Want::Register(m), Want::Off);
        assert_eq!(hotkeys.state(Action::ToggleMic), &State::Failed("another app already uses it".into()));

        *fake.refuse.borrow_mut() = false;
        sync(&mut hotkeys, Want::Register(m), Want::Off);
        assert!(fake.live.borrow().is_empty());
        hotkeys.retry_failed();
        sync(&mut hotkeys, Want::Register(m), Want::Off);
        assert_eq!(hotkeys.state(Action::ToggleMic), &State::Active);

        sync(&mut hotkeys, Want::Refused("F11 also opens full screen in the Screens window".into()), Want::Off);
        assert!(fake.live.borrow().is_empty());
        assert!(matches!(hotkeys.state(Action::ToggleMic), State::Failed(_)));
    }
}
