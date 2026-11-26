use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};
use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use t7_proto::ClientId;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{CursorIcon, Fullscreen, Theme, WindowId};

use crate::bindings::{Action as KeyAction, Bindings, Chord};
use crate::icons::{self, Icon};
use crate::net::ScreenSink;
use crate::screen::scale::{Area, Scaler};
use crate::screen::{AccessUnit, Decoder, Picture};
use crate::theme;

pub type Wake = Arc<dyn Fn() + Send + Sync>;

const QUEUE: usize = 4;

const DOUBLE_CLICK: Duration = Duration::from_millis(500);

const CONTROLS_LINGER: Duration = Duration::from_secs(2);

const PAD_PT: f32 = 8.0;
const STRIP_HEIGHT_PT: f32 = 92.0;

const BACKGROUND: u32 = 0x0010_1010;

#[derive(Default)]
pub struct Frames(Mutex<Slots>);

#[derive(Default)]
struct Slots {
    newest: Option<Picture>,
    spare: Option<Picture>,
}

impl Frames {
    fn put(&self, picture: Picture) -> Option<Picture> {
        let mut slots = self.0.lock().unwrap();
        slots.newest.replace(picture).or_else(|| slots.spare.take())
    }

    fn take_newest(&self, shown: &mut Option<Picture>) -> bool {
        let mut slots = self.0.lock().unwrap();
        let Some(newest) = slots.newest.take() else { return false };
        slots.spare = shown.replace(newest);
        true
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

pub struct Stream {
    pub sharer: ClientId,
    pub name: String,
    pub frames: Arc<Frames>,
}

pub enum Action {
    Close,
    Stop(ClientId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Grid,
    Focus(ClientId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    ShowAll,
    Fullscreen,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Nothing,
    Tile(ClientId),
    Button(ClientId, Button),
}

struct Tile {
    stream: Stream,
    shown: Option<Picture>,
    scaler: Scaler,
    area: Option<Area>,
    redraw: bool,
}

pub struct Window {
    winit: Rc<winit::window::Window>,
    input: egui_winit::State,
    surface: softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>,
    tiles: Vec<Tile>,
    layout: Layout,
    pointer_px: Option<Pos2>,
    controls_until: Option<Instant>,
    pressed: Option<Instant>,
    cursor: CursorIcon,
    overlay: Option<Overlay>,
    stale: bool,
    title: String,
}

impl Window {
    pub fn open(event_loop: &ActiveEventLoop, streams: Vec<Stream>) -> Result<Window, String> {
        let attrs = winit::window::Window::default_attributes()
            .with_title("T7")
            .with_window_icon(crate::brand::window_icon())
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(960.0, 540.0))
            .with_min_inner_size(LogicalSize::new(480.0, 270.0));
        let window = Rc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|cx| softbuffer::Surface::new(&cx, window.clone()))
            .map_err(|e| e.to_string())?;
        let mut input = egui_winit::State::new(
            egui::Context::default(),
            egui::ViewportId::ROOT,
            &window,
            None,
            Some(Theme::Dark),
            None,
        );
        input.set_clipboard_shortcuts(false);
        let mut window = Window {
            winit: window,
            input,
            surface,
            tiles: Vec::new(),
            layout: Layout::Grid,
            pointer_px: None,
            controls_until: None,
            pressed: None,
            cursor: CursorIcon::Default,
            overlay: None,
            stale: true,
            title: String::new(),
        };
        window.show(streams);
        Ok(window)
    }

    pub fn id(&self) -> WindowId {
        self.winit.id()
    }

    pub fn show(&mut self, streams: Vec<Stream>) {
        let same = |t: &Tile, s: &Stream| t.stream.sharer == s.sharer && Arc::ptr_eq(&t.stream.frames, &s.frames);
        let unchanged = streams.len() == self.tiles.len() && self.tiles.iter().zip(&streams).all(|(t, s)| same(t, s));
        if unchanged {
            for (tile, stream) in self.tiles.iter_mut().zip(streams) {
                if tile.stream.name != stream.name {
                    tile.stream.name = stream.name;
                    tile.redraw = true;
                    self.winit.request_redraw();
                }
            }
        } else {
            let mut old = std::mem::take(&mut self.tiles);
            let added = streams.iter().any(|s| !old.iter().any(|t| same(t, s)));
            self.tiles = streams
                .into_iter()
                .map(|stream| match old.iter().position(|t| same(t, &stream)) {
                    Some(i) => Tile { stream, ..old.swap_remove(i) },
                    None => Tile { stream, shown: None, scaler: Scaler::default(), area: None, redraw: true },
                })
                .collect();
            if let Layout::Focus(sharer) = self.layout
                && !self.tiles.iter().any(|t| t.stream.sharer == sharer)
            {
                self.layout = Layout::Grid;
            }
            self.restale();
            if added {
                self.winit.focus_window();
            }
        }
        let title = match self.tiles.as_slice() {
            [one] => format!("{}'s screen — T7", one.stream.name),
            _ => "Screens — T7".to_owned(),
        };
        if title != self.title {
            self.winit.set_title(&title);
            self.title = title;
        }
    }

    pub fn wake(&self) {
        self.winit.request_redraw();
    }

    pub fn wake_at(&self) -> Option<Instant> {
        self.controls_until
    }

    pub fn hide_idle_controls(&mut self, now: Instant) {
        if self.controls_until.is_some_and(|at| at <= now) {
            self.controls_until = None;
            self.redraw_all();
            self.winit.set_cursor_visible(!self.fullscreen());
            self.set_cursor();
        }
    }

    pub fn event(&mut self, event: &WindowEvent, bindings: &Bindings, capturing: bool) -> Option<Action> {
        match event {
            WindowEvent::CloseRequested => return Some(Action::Close),
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.restale(),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer_moved(Some(pos2(position.x as f32, position.y as f32)));
            }
            WindowEvent::CursorLeft { .. } => self.pointer_moved(None),
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                return self.press();
            }
            WindowEvent::KeyboardInput { .. } | WindowEvent::ModifiersChanged(_) | WindowEvent::Focused(_) => {
                let _ = self.input.on_window_event(&self.winit, event);
                let events = std::mem::take(&mut self.input.egui_input_mut().events);
                if !capturing
                    && matches!(event, WindowEvent::KeyboardInput { event, .. } if !event.repeat)
                    && let Some(action) = events.iter().find_map(|event| fullscreen_action(bindings, event))
                {
                    self.set_fullscreen(action == KeyAction::ToggleFullscreen && !self.fullscreen());
                }
            }
            _ => {}
        }
        None
    }

    fn fullscreen(&self) -> bool {
        self.winit.fullscreen().is_some()
    }

    fn set_fullscreen(&mut self, on: bool) {
        if on != self.fullscreen() {
            self.winit.set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
            self.restale();
        }
    }

    fn set_layout(&mut self, layout: Layout) {
        if self.layout != layout {
            self.layout = layout;
            self.restale();
        }
    }

    fn restale(&mut self) {
        self.stale = true;
        self.winit.request_redraw();
    }

    fn redraw_all(&mut self) {
        self.tiles.iter_mut().for_each(|t| t.redraw = true);
        self.winit.request_redraw();
    }

    fn redraw(&mut self, target: Target) {
        let sharer = match target {
            Target::Nothing => return,
            Target::Tile(sharer) | Target::Button(sharer, _) => sharer,
        };
        if let Some(tile) = self.tiles.iter_mut().find(|t| t.stream.sharer == sharer) {
            tile.redraw = true;
            self.winit.request_redraw();
        }
    }

    fn pointer_moved(&mut self, pointer_px: Option<Pos2>) {
        let (target, shown) = (self.target(), self.controls_until.is_some());
        self.pointer_px = pointer_px;
        self.controls_until = pointer_px.map(|_| Instant::now() + CONTROLS_LINGER);
        if pointer_px.is_some() && !shown {
            self.winit.set_cursor_visible(true);
        }
        if shown != self.controls_until.is_some() {
            self.redraw_all();
        } else if target != self.target() {
            self.redraw(target);
            self.redraw(self.target());
        }
        self.set_cursor();
    }

    fn press(&mut self) -> Option<Action> {
        let now = Instant::now();
        let double = self.pressed.is_some_and(|at| now.duration_since(at) < DOUBLE_CLICK);
        self.pressed = (!double).then_some(now);
        match self.target() {
            Target::Button(sharer, button) => {
                self.pressed = None;
                match button {
                    Button::Stop => return Some(Action::Stop(sharer)),
                    Button::Fullscreen => self.set_fullscreen(!self.fullscreen()),
                    Button::ShowAll => self.set_layout(Layout::Grid),
                }
            }
            Target::Tile(sharer) => {
                if self.tiles.len() > 1 {
                    self.set_layout(Layout::Focus(sharer));
                }
                if double {
                    self.set_fullscreen(!self.fullscreen());
                }
            }
            Target::Nothing if double => self.set_fullscreen(!self.fullscreen()),
            Target::Nothing => {}
        }
        self.set_cursor();
        None
    }

    fn set_cursor(&mut self) {
        let clickable = match self.target() {
            Target::Button(..) => true,
            Target::Tile(sharer) => self.tiles.len() > 1 && self.layout != Layout::Focus(sharer),
            Target::Nothing => false,
        };
        let cursor = if clickable { CursorIcon::Pointer } else { CursorIcon::Default };
        if cursor != self.cursor {
            self.cursor = cursor;
            self.winit.set_cursor(cursor);
        }
    }

    fn ppp(&self) -> f32 {
        self.winit.scale_factor() as f32
    }

    fn target(&self) -> Target {
        let Some(pointer_px) = self.pointer_px else { return Target::Nothing };
        let ppp = self.ppp();
        let pointer = pointer_px / ppp;
        let hovered = self.tiles.iter().find_map(|t| {
            let area = rect_of(t.area?, ppp);
            area.contains(pointer).then_some((t.stream.sharer, area))
        });
        let Some((sharer, area)) = hovered else { return Target::Nothing };
        let buttons = if self.controls_until.is_some() { buttons(self.layout, self.tiles.len(), sharer) } else { &[] };
        let hit = buttons.iter().enumerate().find(|&(i, _)| button_rect_from_right(area, i).contains(pointer));
        hit.map_or(Target::Tile(sharer), |(_, &button)| Target::Button(sharer, button))
    }

    fn paint(&mut self) {
        let size = self.winit.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if let Err(e) = self.surface.resize(w, h) {
            return tracing::error!("cannot resize the screens window: {e}");
        }
        let (width, height) = (size.width as usize, size.height as usize);
        let stale = std::mem::take(&mut self.stale);
        if stale {
            let focus = match self.layout {
                Layout::Focus(sharer) => self.tiles.iter().position(|t| t.stream.sharer == sharer),
                Layout::Grid => None,
            };
            let strip = if self.fullscreen() { Strip::Hidden } else { Strip::Shown };
            let areas = arrange((width, height), self.ppp(), self.tiles.len(), focus, strip);
            for (tile, area) in self.tiles.iter_mut().zip(areas) {
                tile.area = area;
            }
        }
        let look = self.look();
        for tile in &mut self.tiles {
            tile.redraw |= tile.stream.frames.take_newest(&mut tile.shown);
        }

        let mut buffer = match self.surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(e) => return tracing::error!("cannot map the screens window: {e}"),
        };
        let stale = stale || buffer.age() == 0;
        if stale {
            buffer.fill(BACKGROUND);
            self.tiles.iter_mut().for_each(|t| t.redraw = true);
        }
        let mut damage = Vec::new();
        for tile in self.tiles.iter_mut().filter(|t| t.redraw) {
            let Some(area) = tile.area else { continue };
            match &tile.shown {
                Some(picture) => tile.scaler.draw(picture, &mut buffer, width, area),
                None => area.clear(&mut buffer, width),
            }
            damage.push(damage_rect(area));
        }
        let dressed = |t: &Tile| t.redraw && t.area.is_some() && (look.controls || t.shown.is_none());
        if self.tiles.iter().any(dressed) {
            let overlay = self.overlay.get_or_insert_with(Overlay::new);
            overlay.paint(&mut buffer, (width, height), look.ppp, |p| {
                self.tiles.iter().filter(|t| dressed(t)).for_each(|tile| dress(p, tile, look));
            });
        }
        self.tiles.iter_mut().for_each(|t| t.redraw = false);

        let system_repaint = damage.is_empty();
        let presented = if stale || system_repaint { buffer.present() } else { buffer.present_with_damage(&damage) };
        if let Err(e) = presented {
            tracing::error!("cannot present the screens window: {e}");
        }
    }

    fn look(&self) -> Look {
        Look {
            layout: self.layout,
            count: self.tiles.len(),
            controls: self.controls_until.is_some(),
            fullscreen: self.fullscreen(),
            target: self.target(),
            ppp: self.ppp(),
        }
    }
}

fn fullscreen_action(bindings: &Bindings, event: &egui::Event) -> Option<KeyAction> {
    let (chord, false) = Chord::from_press(event)? else { return None };
    [KeyAction::ToggleFullscreen, KeyAction::ExitFullscreen]
        .into_iter()
        .find(|&action| bindings.chord(action) == Some(chord))
}

#[derive(Clone, Copy)]
struct Look {
    layout: Layout,
    count: usize,
    controls: bool,
    fullscreen: bool,
    target: Target,
    ppp: f32,
}

fn buttons(layout: Layout, count: usize, sharer: ClientId) -> &'static [Button] {
    match layout {
        _ if count == 1 => &[Button::Stop, Button::Fullscreen],
        Layout::Focus(focused) if focused == sharer => &[Button::Stop, Button::Fullscreen, Button::ShowAll],
        Layout::Focus(_) => &[],
        Layout::Grid => &[Button::Stop],
    }
}

fn dress(p: &egui::Painter, tile: &Tile, look: Look) {
    let Some(area) = tile.area else { return };
    let rect = rect_of(area, look.ppp);
    let p = p.with_clip_rect(rect);
    let (sharer, name) = (tile.stream.sharer, &tile.stream.name);
    if tile.shown.is_none() {
        let wait = format!("Waiting for {name}'s screen…");
        p.text(rect.center(), Align2::CENTER_CENTER, wait, FontId::proportional(13.5), theme::TEXT_DIM);
    }
    if !look.controls {
        return;
    }
    name_tag(&p, rect, name, rect.height() < 2.0 * STRIP_HEIGHT_PT);

    let hovered = matches!(look.target, Target::Tile(s) | Target::Button(s, _) if s == sharer);
    let buttons = buttons(look.layout, look.count, sharer);
    if buttons.is_empty() || (look.layout == Layout::Grid && look.count > 1 && !hovered) {
        return;
    }
    let group = button_rect_from_right(rect, 0).union(button_rect_from_right(rect, buttons.len() - 1)).expand(6.0);
    p.rect(group, CornerRadius::same(8), CONTROL_BG, Stroke::new(1.0, theme::BORDER_STRONG), StrokeKind::Inside);
    for (i, &button) in buttons.iter().enumerate() {
        let at = button_rect_from_right(rect, i);
        let lit = look.target == Target::Button(sharer, button);
        if lit {
            p.rect_filled(at, CornerRadius::same(6), theme::SURFACE_HOVER);
        }
        let color = if lit { theme::TEXT } else { theme::TEXT_DIM };
        let glyph = Rect::from_center_size(at.center(), vec2(16.0, 16.0));
        match button {
            Button::Stop => icons::paint(&p, glyph, Icon::Cross, color),
            Button::Fullscreen => paint_fullscreen(&p, glyph, look.fullscreen, color),
            Button::ShowAll => paint_grid(&p, glyph, color),
        }
        if lit {
            let tip = match button {
                Button::Stop => "Stop watching",
                Button::Fullscreen if look.fullscreen => "Exit full screen",
                Button::Fullscreen => "Full screen",
                Button::ShowAll => "Show all",
            };
            let galley = p.layout_no_wrap(tip.to_owned(), FontId::proportional(12.5), theme::TEXT);
            let tag = Rect::from_min_size(
                pos2(group.right() - galley.size().x - 16.0, group.bottom() + 6.0),
                galley.size() + vec2(16.0, 8.0),
            );
            p.rect(tag, CornerRadius::same(6), CONTROL_BG, Stroke::new(1.0, theme::BORDER_STRONG), StrokeKind::Inside);
            p.galley(tag.min + vec2(8.0, 4.0), galley, theme::TEXT);
        }
    }
}

const CONTROL_BG: Color32 = Color32::from_rgba_premultiplied(15, 15, 15, 235);

#[derive(Clone, Copy, PartialEq)]
enum Strip {
    Shown,
    Hidden,
}

fn arrange(size: (usize, usize), ppp: f32, count: usize, focus: Option<usize>, strip: Strip) -> Vec<Option<Area>> {
    let (width, height) = size;
    if count <= 1 {
        return vec![Some(Area::whole(width, height)); count];
    }
    let px = |points: f32| (points * ppp).round() as usize;
    let (pad, strip_h) = (px(PAD_PT), px(STRIP_HEIGHT_PT));
    let (inner_w, inner_h) = (width.saturating_sub(2 * pad), height.saturating_sub(2 * pad));
    let area = |x, y, width, height| Some(Area { x, y, width, height });
    match focus {
        Some(focused) if strip == Strip::Hidden || inner_h < 3 * strip_h => {
            (0..count).map(|i| if i == focused { area(0, 0, width, height) } else { None }).collect()
        }
        Some(focused) => {
            let others = count - 1;
            let thumb_w = (strip_h * 16 / 9).min(inner_w.saturating_sub(pad * (others - 1)) / others);
            let strip_y = height - pad - strip_h;
            let mut x = pad;
            (0..count)
                .map(|i| {
                    if i == focused {
                        return area(pad, pad, inner_w, inner_h - pad - strip_h);
                    }
                    let at = x;
                    x += thumb_w + pad;
                    area(at, strip_y, thumb_w, strip_h)
                })
                .collect()
        }
        None => {
            let (cols, rows) = (2, count.div_ceil(2));
            let cell_w = inner_w.saturating_sub(pad) / cols;
            let cell_h = inner_h.saturating_sub(pad * (rows - 1)) / rows;
            (0..count)
                .map(|i| {
                    let (row, col) = (i / cols, i % cols);
                    let in_row = (count - row * cols).min(cols);
                    let indent = (inner_w - (in_row * cell_w + (in_row - 1) * pad)) / 2;
                    area(pad + indent + col * (cell_w + pad), pad + row * (cell_h + pad), cell_w, cell_h)
                })
                .collect()
        }
    }
}

fn rect_of(area: Area, ppp: f32) -> Rect {
    Rect::from_min_size(pos2(area.x as f32, area.y as f32), vec2(area.width as f32, area.height as f32)) / ppp
}

fn damage_rect(area: Area) -> softbuffer::Rect {
    let one = NonZeroU32::MIN;
    softbuffer::Rect {
        x: area.x as u32,
        y: area.y as u32,
        width: NonZeroU32::new(area.width as u32).unwrap_or(one),
        height: NonZeroU32::new(area.height as u32).unwrap_or(one),
    }
}

fn button_rect_from_right(tile: Rect, i: usize) -> Rect {
    let right = tile.right() - 16.0 - i as f32 * 36.0;
    Rect::from_min_max(pos2(right - 30.0, tile.top() + 16.0), pos2(right, tile.top() + 46.0))
}

fn name_tag(p: &egui::Painter, tile: Rect, name: &str, small: bool) {
    let (inset, avatar, pad_l, pad_r, height, size) =
        if small { (6.0, 16.0, 4.0, 8.0, 22.0, 12.0) } else { (10.0, 20.0, 6.0, 10.0, 28.0, 12.5) };
    let max_text = tile.width() - 2.0 * inset - pad_l - avatar - 6.0 - pad_r;
    let mut job =
        LayoutJob::single_section(name.to_owned(), TextFormat::simple(FontId::proportional(size), theme::TEXT));
    job.wrap = TextWrapping::truncate_at_width(max_text.max(0.0));
    let galley = p.layout_job(job);
    let tag = Rect::from_min_size(
        tile.min + vec2(inset, inset),
        vec2(pad_l + avatar + 6.0 + galley.size().x + pad_r, height),
    );
    p.rect(tag, CornerRadius::same(8), CONTROL_BG, Stroke::new(1.0, theme::BORDER_STRONG), StrokeKind::Inside);
    let center = pos2(tag.left() + pad_l + avatar / 2.0, tag.center().y);
    p.circle_filled(center, avatar / 2.0, theme::avatar_color(name));
    let initial: String = name.chars().next().map_or_default(|c| c.to_uppercase().collect());
    p.text(center, Align2::CENTER_CENTER, initial, FontId::proportional(avatar * 0.55), Color32::WHITE);
    let text_at = pos2(center.x + avatar / 2.0 + 6.0, tag.center().y - galley.size().y / 2.0);
    p.galley(text_at, galley, theme::TEXT);
}

fn paint_fullscreen(p: &egui::Painter, rect: Rect, inward: bool, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let arm = rect.width() * 0.35;
    for (corner, dx, dy) in [
        (rect.left_top(), 1.0, 1.0),
        (rect.right_top(), -1.0, 1.0),
        (rect.left_bottom(), 1.0, -1.0),
        (rect.right_bottom(), -1.0, -1.0),
    ] {
        let elbow = if inward { corner + vec2(dx, dy) * arm } else { corner };
        let (h, v) = if inward { (vec2(-dx, 0.0), vec2(0.0, -dy)) } else { (vec2(dx, 0.0), vec2(0.0, dy)) };
        p.line_segment([elbow, elbow + h * arm], stroke);
        p.line_segment([elbow, elbow + v * arm], stroke);
    }
}

fn paint_grid(p: &egui::Painter, rect: Rect, color: Color32) {
    let gap = rect.width() * 0.14;
    let cell = (rect.width() - gap) / 2.0;
    for (col, row) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
        let min = rect.min + vec2(col, row) * (cell + gap);
        p.rect_stroke(
            Rect::from_min_size(min, vec2(cell, cell)),
            CornerRadius::same(2),
            Stroke::new(1.5, color),
            StrokeKind::Inside,
        );
    }
}

struct Overlay {
    ctx: egui::Context,
    renderer: EguiSoftwareRender,
}

impl Overlay {
    fn new() -> Overlay {
        let renderer = EguiSoftwareRender::new(ColorFieldOrder::Bgra).with_caching(false);
        Overlay { ctx: egui::Context::default(), renderer }
    }

    fn paint(&mut self, buffer: &mut [u32], size: (usize, usize), ppp: f32, draw: impl Fn(&egui::Painter)) {
        let (width, height) = size;
        let mut input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width as f32, height as f32) / ppp)),
            ..Default::default()
        };
        input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
        let mut out = self.ctx.run_pass(input, |ctx| draw(&ctx.layer_painter(egui::LayerId::background())));
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let px: &mut [[u8; 4]] = bytemuck::cast_slice_mut(buffer);
        let mut target = BufferMutRef::new(px, width, height);
        self.renderer.render(&mut target, &prims, &out.textures_delta, out.pixels_per_point);
        out.textures_delta.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullscreen_actions_follow_the_live_profile_bindings() {
        let mut bindings = Bindings::default();
        let mut hold = None;
        let down = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert_eq!(fullscreen_action(&bindings, &down(egui::Key::F11)), Some(KeyAction::ToggleFullscreen));
        assert_eq!(fullscreen_action(&bindings, &down(egui::Key::Escape)), Some(KeyAction::ExitFullscreen));
        let f6 = down(egui::Key::F6);
        let binding = crate::bindings::Binding::Chord(Chord::from_press(&f6).unwrap().0);
        bindings.set(KeyAction::ExitFullscreen, &mut hold, Some(binding)).unwrap();
        assert_eq!(fullscreen_action(&bindings, &down(egui::Key::Escape)), None);
        assert_eq!(fullscreen_action(&bindings, &f6), Some(KeyAction::ExitFullscreen));
    }

    fn area(x: usize, y: usize, width: usize, height: usize) -> Area {
        Area { x, y, width, height }
    }

    #[test]
    fn one_screen_fills_the_window() {
        assert_eq!(arrange((960, 540), 1.0, 1, None, Strip::Shown), [Some(area(0, 0, 960, 540))]);
        assert_eq!(arrange((960, 540), 1.0, 1, Some(0), Strip::Shown), [Some(area(0, 0, 960, 540))]);
    }

    #[test]
    fn two_sit_side_by_side_and_three_or_four_in_two_rows() {
        assert_eq!(
            arrange((960, 540), 1.0, 2, None, Strip::Shown),
            [Some(area(8, 8, 468, 524)), Some(area(484, 8, 468, 524))]
        );
        let three = arrange((960, 540), 1.0, 3, None, Strip::Shown);
        assert_eq!(
            three,
            [Some(area(8, 8, 468, 258)), Some(area(484, 8, 468, 258)), Some(area(246, 274, 468, 258))],
            "last one centered"
        );
        let four = arrange((1920, 1080), 2.0, 4, None, Strip::Shown);
        assert_eq!(four[3], Some(area(968, 548, 936, 516)), "in pixels at 200%");
    }

    #[test]
    fn a_focused_screen_fills_the_window_above_a_strip_of_the_others() {
        let focused = arrange((960, 540), 1.0, 3, Some(1), Strip::Shown);
        assert_eq!(focused, [Some(area(8, 440, 163, 92)), Some(area(8, 8, 944, 424)), Some(area(179, 440, 163, 92))]);
        let full = arrange((1920, 1080), 1.0, 3, Some(2), Strip::Hidden);
        assert_eq!(full, [None, None, Some(area(0, 0, 1920, 1080))], "full screen shows only the focused one");
        let low = arrange((960, 270), 1.0, 2, Some(0), Strip::Shown);
        assert_eq!(low, [Some(area(0, 0, 960, 270)), None], "no room for the strip");
    }

    #[test]
    fn a_new_picture_replaces_the_one_shown() {
        let frames = Frames::default();
        let mut shown = None;
        assert!(!frames.take_newest(&mut shown), "nothing yet");
        frames.put(Picture { width: 1, ..Picture::default() });
        assert!(frames.take_newest(&mut shown));
        assert_eq!(shown.as_ref().map(|p| p.width), Some(1));
        assert!(!frames.take_newest(&mut shown), "already shown");
    }
}
