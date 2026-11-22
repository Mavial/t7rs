//! The native window: winit for the window and input, egui for the UI, and a
//! CPU rasterizer drawing into a plain pixel buffer (softbuffer).
//!
//! No GPU driver is loaded. On a typical desktop the OpenGL driver alone
//! commits ~65 MB, several times what this whole client needs, while drawing
//! a mostly static UI on the CPU costs a few milliseconds per repaint, and
//! the UI only repaints on input or network events.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Theme, Window, WindowId};

use crate::app::App;
use crate::settings::Store;

/// Sent by egui, from any thread, when it wants a repaint after the given delay.
struct RepaintAfter(Duration);

struct Gfx {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    input: egui_winit::State,
    renderer: EguiSoftwareRender,
}

struct Shell {
    ctx: egui::Context,
    app: App,
    store: Store,
    gfx: Option<Gfx>,
    next_repaint: Option<Instant>,
    title: String,
}

/// Runs the UI until the window closes. `make_app` runs after the repaint
/// wakeup is wired, so nothing the app starts can miss a repaint.
pub fn run(
    ctx: egui::Context,
    store: Store,
    make_app: impl FnOnce(&egui::Context) -> App,
) -> Result<(), winit::error::EventLoopError> {
    let event_loop = EventLoop::<RepaintAfter>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    ctx.set_request_repaint_callback(move |info| {
        let _ = proxy.send_event(RepaintAfter(info.delay));
    });
    let app = make_app(&ctx);
    let mut shell = Shell { ctx, app, store, gfx: None, next_repaint: None, title: String::new() };
    event_loop.run_app(&mut shell)
}

impl Shell {
    fn schedule(&mut self, delay: Duration) {
        if delay.is_zero() {
            if let Some(g) = &self.gfx {
                g.window.request_redraw();
            }
        } else if let Some(at) = Instant::now().checked_add(delay) {
            self.next_repaint = Some(self.next_repaint.map_or(at, |t| t.min(at)));
        }
    }

    fn paint(&mut self) {
        let Some(g) = &mut self.gfx else { return };
        let started = Instant::now();

        let raw = g.input.take_egui_input(&g.window);
        let app = &mut self.app;
        let out = self.ctx.run_ui(raw, |ui| app.ui(ui));
        g.input.handle_platform_output(&g.window, out.platform_output);

        if self.app.take_attention() && !g.window.has_focus() {
            g.window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
        }
        let title = self.app.title();
        if title != self.title {
            g.window.set_title(&title);
            self.title = title;
        }
        self.store.save(self.app.settings());

        let size = g.window.inner_size();
        if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
            let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
            if let Err(e) = g.surface.resize(w, h) {
                return tracing::error!("cannot resize surface: {e}");
            }
            match g.surface.buffer_mut() {
                Ok(mut buf) => {
                    let px: &mut [[u8; 4]] = bytemuck::cast_slice_mut(&mut buf);
                    let mut target = BufferMutRef::new(px, size.width as usize, size.height as usize);
                    g.renderer.render(&mut target, &prims, &out.textures_delta, out.pixels_per_point);
                    if let Err(e) = buf.present() {
                        tracing::error!("cannot present frame: {e}");
                    }
                }
                Err(e) => tracing::error!("cannot map surface: {e}"),
            }
        }
        tracing::debug!(elapsed = ?started.elapsed(), "frame");

        let delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |v| v.repaint_delay);
        self.schedule(delay);
    }
}

impl ApplicationHandler<RepaintAfter> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("T7")
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(1100.0, 720.0))
            .with_min_inner_size(LogicalSize::new(760.0, 480.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                tracing::error!("cannot create window: {e}");
                return event_loop.exit();
            }
        };
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|cx| softbuffer::Surface::new(&cx, window.clone()));
        let surface = match surface {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("cannot create drawing surface: {e}");
                return event_loop.exit();
            }
        };
        let input = egui_winit::State::new(
            self.ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            Some(Theme::Dark),
            None,
        );
        // Caching keeps a second full-window canvas (~4 bytes per pixel) to
        // re-raster only dirty tiles. Repaints are rare, so memory wins.
        let renderer = EguiSoftwareRender::new(ColorFieldOrder::Bgra).with_caching(false);
        window.request_redraw();
        self.gfx = Some(Gfx { window, surface, input, renderer });
    }

    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            self.next_repaint = None;
            self.schedule(Duration::ZERO);
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, RepaintAfter(delay): RepaintAfter) {
        self.schedule(delay);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(g) = &mut self.gfx else { return };
        let response = g.input.on_window_event(&g.window, &event);
        match event {
            WindowEvent::CloseRequested => {
                self.store.save(self.app.settings());
                self.app.on_exit();
                return event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                self.paint();
                if self.app.take_quit() {
                    self.store.save(self.app.settings());
                    self.app.on_exit();
                    event_loop.exit();
                }
                return;
            }
            _ => {}
        }
        if response.repaint {
            g.window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(match self.next_repaint {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }
}
