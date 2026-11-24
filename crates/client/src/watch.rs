//! Watching someone's screen. Their frames go from the network thread to a
//! decode thread of the watch's own, which puts up the newest picture and
//! wakes the window showing it. The main window does not repaint for video.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Theme, WindowId};

use crate::net::ScreenSink;
use crate::screen::scale::Scaler;
use crate::screen::{AccessUnit, Decoder, Picture};

/// Asks the window showing a screen to draw its newest picture.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Frames waiting for the decoder: ~130 ms at 30 fps. When it falls further
/// behind, the frames that don't fit are dropped and the screen starts again
/// from a keyframe.
const QUEUE: usize = 4;

/// Two clicks this close together are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// The newest picture of a watched screen, on its way from the decode thread to the window.
#[derive(Default)]
pub struct Frames(Mutex<Slots>);

#[derive(Default)]
struct Slots {
    /// Decoded and not drawn yet.
    newest: Option<Picture>,
    /// Drawn and replaced since, for the decoder to reuse.
    spare: Option<Picture>,
}

impl Frames {
    /// Puts up `picture` to draw, and returns a picture to reuse if there is one.
    fn put(&self, picture: Picture) -> Option<Picture> {
        let mut slots = self.0.lock().unwrap();
        slots.newest.replace(picture).or_else(|| slots.spare.take())
    }

    /// Swaps the picture put up since the last call, if any, into `shown`.
    fn take_newest(&self, shown: &mut Option<Picture>) {
        let mut slots = self.0.lock().unwrap();
        if let Some(newest) = slots.newest.take() {
            slots.spare = shown.replace(newest);
        }
    }
}

/// Starts decoding a screen on its own thread. The returned sink goes to the
/// net layer; the thread ends when the sink is dropped.
pub fn start(wake: Wake) -> Result<(Arc<Frames>, ScreenSink), String> {
    let decoder = Decoder::new()?;
    let frames = Arc::new(Frames::default());
    let failed = Arc::new(AtomicBool::new(false));
    let (queue, units) = mpsc::sync_channel(QUEUE);
    std::thread::Builder::new()
        .name("screen decoder".into())
        .spawn({
            let (frames, failed) = (frames.clone(), failed.clone());
            move || decode(&units, decoder, &frames, &failed, &*wake)
        })
        .map_err(|e| format!("screen decoder thread: {e}"))?;
    let sink: ScreenSink = Box::new(move |au| {
        // Refusing a frame makes the net layer ask for a keyframe.
        let failed = failed.swap(false, Relaxed);
        queue.try_send(au).is_ok() && !failed
    });
    Ok((frames, sink))
}

fn decode(units: &Receiver<AccessUnit>, mut decoder: Decoder, frames: &Frames, failed: &AtomicBool, wake: &dyn Fn()) {
    while let Ok(au) = units.recv() {
        match decoder.decode(&au) {
            Ok(Some(picture)) => {
                if let Some(old) = frames.put(picture) {
                    decoder.recycle(old);
                }
                wake();
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("screen decode: {e}");
                failed.store(true, Relaxed);
            }
        }
    }
}

/// The window showing the screen watched. It draws only when a new picture
/// arrives or it changes size.
pub struct Window {
    winit: Rc<winit::window::Window>,
    surface: softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>,
    frames: Arc<Frames>,
    shown: Option<Picture>,
    scaler: Scaler,
    title: String,
    /// When the left button last went down, to tell a double-click.
    pressed: Option<Instant>,
}

impl Window {
    pub fn open(event_loop: &ActiveEventLoop, frames: Arc<Frames>, title: String) -> Result<Window, String> {
        let attrs = winit::window::Window::default_attributes()
            .with_title(&title)
            .with_window_icon(crate::brand::window_icon())
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(960.0, 540.0))
            .with_min_inner_size(LogicalSize::new(320.0, 180.0));
        let window = Rc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|cx| softbuffer::Surface::new(&cx, window.clone()))
            .map_err(|e| e.to_string())?;
        Ok(Window { winit: window, surface, frames, shown: None, scaler: Scaler::default(), title, pressed: None })
    }

    pub fn id(&self) -> WindowId {
        self.winit.id()
    }

    /// Shows `frames` under `title` from now on, keeping the window where it is.
    pub fn show(&mut self, frames: &Arc<Frames>, title: &str) {
        if !Arc::ptr_eq(frames, &self.frames) {
            self.frames = frames.clone();
            self.shown = None;
            self.winit.request_redraw();
        }
        if title != self.title {
            self.winit.set_title(title);
            title.clone_into(&mut self.title);
        }
    }

    /// A new picture is ready.
    pub fn wake(&self) {
        self.winit.request_redraw();
    }

    /// Handles one of its events; true once the user closes it.
    pub fn event(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::CloseRequested => return true,
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::Resized(_) => self.winit.request_redraw(),
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let now = Instant::now();
                if self.pressed.is_some_and(|at| now.duration_since(at) < DOUBLE_CLICK) {
                    self.pressed = None;
                    self.set_fullscreen(self.winit.fullscreen().is_none());
                } else {
                    self.pressed = Some(now);
                }
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent { logical_key, state: ElementState::Pressed, repeat: false, .. },
                ..
            } => match logical_key {
                Key::Named(NamedKey::F11) => self.set_fullscreen(self.winit.fullscreen().is_none()),
                Key::Named(NamedKey::Escape) => self.set_fullscreen(false),
                _ => {}
            },
            _ => {}
        }
        false
    }

    fn set_fullscreen(&self, on: bool) {
        self.winit.set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
    }

    fn paint(&mut self) {
        self.frames.take_newest(&mut self.shown);
        let size = self.winit.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if let Err(e) = self.surface.resize(w, h) {
            return tracing::error!("cannot resize the screen window: {e}");
        }
        let mut buffer = match self.surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(e) => return tracing::error!("cannot map the screen window: {e}"),
        };
        match &self.shown {
            Some(picture) => self.scaler.draw(picture, &mut buffer, size.width as usize, size.height as usize),
            None => buffer.fill(0),
        }
        if let Err(e) = buffer.present() {
            tracing::error!("cannot present the screen window: {e}");
        }
    }
}
