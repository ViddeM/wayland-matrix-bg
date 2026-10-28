//! One background layer surface per output, paced by frame callbacks plus a timer.

use std::error::Error;
use std::time::{Duration, Instant};

use khronos_egl as egl;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, registry_handlers,
};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_egl::WlEglSurface;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::gl::{Frame, Renderer};
use crate::Args;

/// Wrap the shader clock so f32 time keeps sub-millisecond precision.
const TIME_WRAP_SECS: f64 = 3600.0;
/// Draw now if the next frame is due within this much, instead of arming a timer.
const FRAME_SLACK: Duration = Duration::from_millis(2);

struct Wallpaper {
    name: String,
    output: WlOutput,
    layer: LayerSurface,
    /// Both set when the compositor supports fractional scaling (and viewporter).
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    egl: Option<(WlEglSurface, egl::Surface)>,
    /// Size from the layer configure, in logical pixels; (0, 0) until configured.
    logical: (u32, u32),
    /// Scale in 120ths, as in wp_fractional_scale_v1.
    scale120: u32,
    seed: u32,
    last_frame: Option<Instant>,
    frame_pending: bool,
    timer_pending: bool,
}

impl Wallpaper {
    fn physical_size(&self) -> (i32, i32) {
        let (w, h) = self.logical;
        if self.fractional.is_some() {
            let s = self.scale120;
            (((w * s + 60) / 120) as i32, ((h * s + 60) / 120) as i32)
        } else {
            let s = (self.scale120 / 120).max(1);
            ((w * s) as i32, (h * s) as i32)
        }
    }

    /// Physical pixels per logical pixel, matching `physical_size`.
    fn scale(&self) -> f32 {
        if self.fractional.is_some() {
            self.scale120 as f32 / 120.0
        } else {
            (self.scale120 / 120).max(1) as f32
        }
    }

    fn destroy(self, renderer: &Renderer) {
        // EGL surface, then wl_egl_window, then the wl_surface (dropped with `layer`).
        if let Some((window, surface)) = self.egl {
            renderer.destroy_surface(surface);
            drop(window);
        }
        if let Some(viewport) = self.viewport {
            viewport.destroy();
        }
        if let Some(fractional) = self.fractional {
            fractional.destroy();
        }
    }
}

pub struct App {
    pub exit: bool,
    registry_state: RegistryState,
    compositor: CompositorState,
    output_state: OutputState,
    layer_shell: LayerShell,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    renderer: Renderer,
    wallpapers: Vec<Wallpaper>,
    loop_handle: LoopHandle<'static, App>,
    args: Args,
    start: Instant,
}

impl App {
    pub fn new(
        globals: &GlobalList,
        qh: &QueueHandle<Self>,
        loop_handle: LoopHandle<'static, App>,
        renderer: Renderer,
        args: Args,
    ) -> Result<Self, Box<dyn Error>> {
        let compositor = CompositorState::bind(globals, qh)?;
        let layer_shell =
            LayerShell::bind(globals, qh).map_err(|e| format!("wlr-layer-shell: {e}"))?;
        // Fractional scaling needs both; otherwise fall back to integer buffer scale.
        let (fractional_manager, viewporter) =
            match (globals.bind(qh, 1..=1, ()), globals.bind(qh, 1..=1, ())) {
                (Ok(f), Ok(v)) => (Some(f), Some(v)),
                _ => (None, None),
            };

        Ok(Self {
            exit: false,
            registry_state: RegistryState::new(globals),
            compositor,
            output_state: OutputState::new(globals, qh),
            layer_shell,
            fractional_manager,
            viewporter,
            renderer,
            wallpapers: Vec::new(),
            loop_handle,
            args,
            start: Instant::now(),
        })
    }

    fn index_of(&self, surface: &WlSurface) -> Option<usize> {
        self.wallpapers
            .iter()
            .position(|w| w.layer.wl_surface() == surface)
    }

    fn remove(&mut self, idx: usize) {
        let wp = self.wallpapers.swap_remove(idx);
        wp.destroy(&self.renderer);
    }

    /// Sizes the EGL window and viewport for the current logical size and scale.
    fn apply_size(&mut self, idx: usize) -> Result<(), Box<dyn Error>> {
        let wp = &mut self.wallpapers[idx];
        let (w, h) = wp.physical_size();
        let surface = wp.layer.wl_surface();
        match &wp.egl {
            Some((window, _)) => window.resize(w, h, 0, 0),
            None => {
                let window = WlEglSurface::new(surface.id(), w, h)?;
                let egl_surface = self.renderer.create_surface(&window)?;
                wp.egl = Some((window, egl_surface));
            }
        }
        match &wp.viewport {
            Some(viewport) => viewport.set_destination(wp.logical.0 as i32, wp.logical.1 as i32),
            None => surface.set_buffer_scale(wp.scale() as i32),
        }
        if let Ok(region) = Region::new(&self.compositor) {
            region.add(0, 0, wp.logical.0 as i32, wp.logical.1 as i32);
            surface.set_opaque_region(Some(region.wl_region()));
        }
        Ok(())
    }

    /// Draws now if a frame is due, else arms a timer; no-op while a frame or timer is pending.
    fn schedule(&mut self, idx: usize, qh: &QueueHandle<Self>) {
        let wp = &mut self.wallpapers[idx];
        if wp.frame_pending || wp.timer_pending || wp.egl.is_none() {
            return;
        }
        let due = wp.last_frame.map_or(Duration::ZERO, |t| {
            (t + self.args.frame_interval()).saturating_duration_since(Instant::now())
        });
        if due <= FRAME_SLACK {
            self.draw(idx, qh);
            return;
        }

        let surface = wp.layer.wl_surface().clone();
        let timer_qh = qh.clone();
        let inserted =
            self.loop_handle
                .insert_source(Timer::from_duration(due), move |_, _, app| {
                    if let Some(idx) = app.index_of(&surface) {
                        app.wallpapers[idx].timer_pending = false;
                        app.schedule(idx, &timer_qh);
                    }
                    TimeoutAction::Drop
                });
        match inserted {
            Ok(_) => wp.timer_pending = true,
            Err(e) => {
                eprintln!("wayland-matrix-bg: failed to arm frame timer: {e}");
                self.draw(idx, qh);
            }
        }
    }

    fn draw(&mut self, idx: usize, qh: &QueueHandle<Self>) {
        let wp = &mut self.wallpapers[idx];
        let Some((_, egl_surface)) = wp.egl else {
            return;
        };
        let (width, height) = wp.physical_size();
        let frame = Frame {
            time: (self.start.elapsed().as_secs_f64() % TIME_WRAP_SECS) as f32,
            width,
            height,
            scale: wp.scale(),
            seed: wp.seed,
        };
        // The frame request is committed by eglSwapBuffers.
        let surface = wp.layer.wl_surface();
        surface.frame(qh, surface.clone());
        wp.last_frame = Some(Instant::now());
        match self.renderer.draw(egl_surface, &frame) {
            Ok(()) => wp.frame_pending = true,
            Err(e) => eprintln!("wayland-matrix-bg: draw failed on {}: {e}", wp.name),
        }
    }

    fn set_scale(&mut self, surface: &WlSurface, scale120: u32, qh: &QueueHandle<Self>) {
        let Some(idx) = self.index_of(surface) else {
            return;
        };
        let wp = &mut self.wallpapers[idx];
        if wp.scale120 == scale120 {
            return;
        }
        wp.scale120 = scale120;
        if wp.logical.0 > 0 {
            if let Err(e) = self.apply_size(idx) {
                eprintln!("wayland-matrix-bg: rescale failed: {e}");
            }
            self.schedule(idx, qh);
        }
    }
}

/// FNV-1a, so each output gets its own (stable) rain pattern.
fn hash_name(name: &str) -> u32 {
    name.bytes().fold(0x811c9dc5u32, |h, b| {
        (h ^ b as u32).wrapping_mul(0x01000193)
    })
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, output: WlOutput) {
        let info = self.output_state.info(&output);
        let name = info
            .as_ref()
            .and_then(|i| i.name.clone())
            .unwrap_or_default();
        let int_scale = info.map_or(1, |i| i.scale_factor.max(1) as u32);

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some(self.args.namespace.clone()),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        if let Ok(region) = Region::new(&self.compositor) {
            layer
                .wl_surface()
                .set_input_region(Some(region.wl_region()));
        }
        let wl_surface = layer.wl_surface();
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(wl_surface, qh, ()));
        let fractional = self
            .fractional_manager
            .as_ref()
            .map(|m| m.get_fractional_scale(wl_surface, qh, wl_surface.clone()));
        layer.commit();

        self.wallpapers.push(Wallpaper {
            seed: hash_name(&name),
            name,
            output,
            layer,
            viewport,
            fractional,
            egl: None,
            logical: (0, 0),
            scale120: int_scale * 120,
            last_frame: None,
            frame_pending: false,
            timer_pending: false,
        });
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        if let Some(idx) = self.wallpapers.iter().position(|w| w.output == output) {
            self.remove(idx);
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(idx) = self.index_of(layer.wl_surface()) {
            self.remove(idx);
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(idx) = self.index_of(layer.wl_surface()) else {
            return;
        };
        let (w, h) = configure.new_size;
        if w == 0 || h == 0 || self.wallpapers[idx].logical == (w, h) {
            return;
        }
        self.wallpapers[idx].logical = (w, h);
        if let Err(e) = self.apply_size(idx) {
            eprintln!(
                "wayland-matrix-bg: failed to set up {}: {e}",
                self.wallpapers[idx].name
            );
            return;
        }
        self.schedule(idx, qh);
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        surface: &WlSurface,
        new_factor: i32,
    ) {
        // Integer fallback only; wp_fractional_scale_v1 wins when available.
        if self.fractional_manager.is_none() {
            self.set_scale(surface, new_factor.max(1) as u32 * 120, qh);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, surface: &WlSurface, _: u32) {
        if let Some(idx) = self.index_of(surface) {
            self.wallpapers[idx].frame_pending = false;
            self.schedule(idx, qh);
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl Dispatch<WpFractionalScaleV1, WlSurface> for App {
    fn event(
        app: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        surface: &WlSurface,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            app.set_scale(surface, scale, qh);
        }
    }
}

/// Interfaces without events.
macro_rules! no_events {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for App {
            fn event(
                _: &mut Self,
                _: &$iface,
                _: <$iface as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}
no_events!(WpFractionalScaleManagerV1, WpViewporter, WpViewport);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_layer!(App);
delegate_registry!(App);
