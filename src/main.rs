mod atlas;
mod gl;
mod wayland;

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use wayland_client::globals::registry_queue_init;
use wayland_client::Connection;

/// Matrix digital rain wallpaper for wlr-layer-shell compositors.
#[derive(Parser, Debug, Clone)]
#[command(version, about)]
pub struct Args {
    /// Frames per second per output.
    #[arg(long, default_value_t = 30.0, value_parser = parse_fps)]
    pub fps: f64,

    /// Rain colour as #rrggbb.
    #[arg(long, default_value = "#00ff41", value_parser = parse_color)]
    pub color: [f32; 3],

    /// Glyph cell height in logical pixels.
    #[arg(long, default_value_t = 18.0)]
    pub cell_size: f32,

    /// Fall speed multiplier.
    #[arg(long, default_value_t = 1.0)]
    pub speed: f32,

    /// Font file, optionally with a face index (path[:index]).
    /// Defaults to fc-match 'Noto Sans Mono CJK JP'.
    #[arg(long)]
    pub font: Option<String>,

    /// Layer-shell namespace (niri layer-rules can match on it).
    #[arg(long, default_value = "wallpaper")]
    pub namespace: String,

    /// Show the local time (24-hour HH:MM) in the rain, centred on each output.
    #[arg(long)]
    pub clock: bool,

    /// Clock digit height as a fraction of the output height.
    #[arg(long, default_value_t = 0.2, value_parser = parse_clock_size, requires = "clock")]
    pub clock_size: f32,

    /// Blink the clock's colon once a second.
    #[arg(long, requires = "clock")]
    pub clock_blink: bool,
}

impl Args {
    pub fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps)
    }
}

fn parse_fps(s: &str) -> Result<f64, String> {
    let fps: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if fps > 0.0 && fps <= 1000.0 {
        Ok(fps)
    } else {
        Err("must be in (0, 1000]".into())
    }
}

fn parse_clock_size(s: &str) -> Result<f32, String> {
    let size: f32 = s.parse().map_err(|e| format!("{e}"))?;
    if size > 0.0 && size <= 1.0 {
        Ok(size)
    } else {
        Err("must be in (0, 1]".into())
    }
}

fn parse_color(s: &str) -> Result<[f32; 3], String> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return Err("expected #rrggbb".into());
    }
    let v = u32::from_str_radix(hex, 16).map_err(|e| format!("{e}"))?;
    Ok([
        ((v >> 16) & 0xff) as f32 / 255.0,
        ((v >> 8) & 0xff) as f32 / 255.0,
        (v & 0xff) as f32 / 255.0,
    ])
}

/// Split `path[:index]`, treating a trailing `:<digits>` as the face index.
pub fn parse_font_spec(spec: &str) -> (PathBuf, u32) {
    if let Some((path, idx)) = spec.rsplit_once(':') {
        if let Ok(idx) = idx.parse() {
            return (PathBuf::from(path), idx);
        }
    }
    (PathBuf::from(spec), 0)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    if !(args.cell_size >= 4.0 && args.cell_size <= 256.0) {
        return Err("--cell-size must be between 4 and 256".into());
    }

    // NVIDIA's driver busy-waits on the GPU by default (~3x the CPU at 30 fps).
    // Must be set before the first EGL call loads the vendor driver.
    if std::env::var_os("__GL_YIELD").is_none() {
        std::env::set_var("__GL_YIELD", "USLEEP");
    }

    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<wayland::App> = EventLoop::try_new()?;
    let renderer = gl::Renderer::new(&conn, &args)?;
    let mut app = wayland::App::new(&globals, &qh, event_loop.handle(), renderer, args)?;

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| format!("failed to insert Wayland source: {}", e.error))?;

    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
    }
    Ok(())
}
