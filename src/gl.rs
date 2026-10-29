//! EGL/GLES setup shared by all outputs and the rain draw call.

use std::error::Error;
use std::ffi::c_void;

use chrono::Timelike;
use glow::HasContext;
use khronos_egl as egl;
use wayland_client::Connection;
use wayland_egl::WlEglSurface;

use crate::{atlas, Args};

const PLATFORM_WAYLAND_KHR: egl::Enum = 0x31D8;

const VERTEX_SHADER: &str = include_str!("fullscreen.vert");
/// Pass 1: rain state per glyph cell, into `cells_texture`.
const CELLS_SHADER: &str = include_str!("cells.frag");
/// Pass 2: full-resolution glyphs from the cell state and the atlas.
const RAIN_SHADER: &str = include_str!("matrix.frag");

/// Clock atlas digit height in pixels; outputs sample it at a coarser mip level.
const CLOCK_ATLAS_HEIGHT: u32 = 128;
/// Width of the clock's colon slot, in digit widths.
const CLOCK_COLON_WIDTH: f32 = 0.6;
/// The clock never gets wider than this fraction of the output.
const CLOCK_MAX_WIDTH: f32 = 0.9;

/// Local wall-clock time to draw, as HH:MM.
pub struct Clock {
    pub digits: [u32; 4],
    pub colon: bool,
}

impl Clock {
    /// With `blink`, the colon shows only in the first half of each second.
    pub fn now(blink: bool) -> Self {
        let t = chrono::Local::now();
        let (h, m) = (t.hour(), t.minute());
        Self {
            digits: [h / 10, h % 10, m / 10, m % 10],
            colon: !blink || t.timestamp_subsec_millis() < 500,
        }
    }
}

/// Per-draw parameters for one output.
pub struct Frame {
    /// Seconds, wrapped to keep f32 precision.
    pub time: f32,
    /// Buffer size in physical pixels.
    pub width: i32,
    pub height: i32,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    pub seed: u32,
    /// Set with `--clock`.
    pub clock: Option<Clock>,
}

struct CellsUniforms {
    time: Option<glow::UniformLocation>,
    rows: Option<glow::UniformLocation>,
    seed: Option<glow::UniformLocation>,
    clock_rect: Option<glow::UniformLocation>,
    clock_digits: Option<glow::UniformLocation>,
    clock_colon: Option<glow::UniformLocation>,
    clock_lod: Option<glow::UniformLocation>,
}

struct RainUniforms {
    resolution: Option<glow::UniformLocation>,
    cell: Option<glow::UniformLocation>,
    lod: Option<glow::UniformLocation>,
}

/// GL objects for the two passes. Shared by all outputs: each draw runs both passes back to back.
struct Pipeline {
    cells_program: glow::Program,
    cells_uniforms: CellsUniforms,
    rain_program: glow::Program,
    rain_uniforms: RainUniforms,
    cells_fbo: glow::Framebuffer,
    /// RGBA8, bound to texture unit 1; grows to the largest output's grid.
    cells_texture: glow::Texture,
}

pub struct Renderer {
    egl: egl::Instance<egl::Static>,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    gl: glow::Context,
    pipeline: Pipeline,
    /// Allocated size of `pipeline.cells_texture`, in cells.
    cells_capacity: (i32, i32),
    cell_size: f32,
    cell_aspect: f32,
    atlas_cell_height: f32,
    /// Digit height as a fraction of the output height (`--clock-size`).
    clock_size: f32,
    /// Clock atlas digit cell width divided by its height.
    clock_aspect: f32,
}

impl Renderer {
    pub fn new(conn: &Connection, args: &Args) -> Result<Self, Box<dyn Error>> {
        let egl = egl::Instance::new(egl::Static);
        let wl_display = conn.backend().display_ptr() as *mut c_void;
        // SAFETY: wl_display is the live libwayland display backing `conn`.
        let display = unsafe {
            egl.get_platform_display(PLATFORM_WAYLAND_KHR, wl_display, &[egl::ATTRIB_NONE])
        }?;
        egl.initialize(display)?;
        egl.bind_api(egl::OPENGL_ES_API)?;

        #[rustfmt::skip]
        let config_attribs = [
            egl::SURFACE_TYPE, egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE, egl::OPENGL_ES3_BIT,
            egl::RED_SIZE, 8,
            egl::GREEN_SIZE, 8,
            egl::BLUE_SIZE, 8,
            egl::ALPHA_SIZE, 0,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &config_attribs)?
            .ok_or("no suitable EGL config")?;
        let context_attribs = [
            egl::CONTEXT_MAJOR_VERSION,
            3,
            egl::CONTEXT_MINOR_VERSION,
            0,
            egl::NONE,
        ];
        let context = egl.create_context(display, config, None, &context_attribs)?;
        // Surfaceless (EGL_KHR_surfaceless_context) so GL resources exist before any output.
        egl.make_current(display, None, None, Some(context))?;

        // SAFETY: the context is current; EGL 1.5 returns core GL symbols too.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map_or(std::ptr::null(), |f| f as *const c_void)
            })
        };

        // Rasterize at ~3x the logical cell height so scale <= 3 outputs never magnify.
        let atlas_cell_height = (args.cell_size * 3.0).round().clamp(32.0, 128.0) as u32;
        let (font, font_label) = atlas::load_font(args.font.as_deref())?;
        let atlas = atlas::build(&font, &font_label, atlas_cell_height)?;
        let clock = args
            .clock
            .then(|| atlas::build_clock(&font, CLOCK_ATLAS_HEIGHT))
            .transpose()?;

        // SAFETY: plain GL calls on the current context.
        let pipeline = unsafe { setup_gl(&gl, &atlas, clock.as_ref(), args)? };

        Ok(Self {
            egl,
            display,
            config,
            context,
            gl,
            pipeline,
            cells_capacity: (0, 0),
            cell_size: args.cell_size,
            cell_aspect: atlas.cell_aspect,
            atlas_cell_height: atlas_cell_height as f32,
            clock_size: args.clock_size,
            clock_aspect: clock.map_or(0.0, |c| c.aspect),
        })
    }

    pub fn create_surface(&self, window: &WlEglSurface) -> Result<egl::Surface, Box<dyn Error>> {
        // SAFETY: `window` is a wl_egl_window on the same Wayland display as `self.display`.
        let surface = unsafe {
            self.egl.create_platform_window_surface(
                self.display,
                self.config,
                window.ptr() as egl::NativeWindowType,
                &[egl::ATTRIB_NONE],
            )
        }?;
        self.egl.make_current(
            self.display,
            Some(surface),
            Some(surface),
            Some(self.context),
        )?;
        // We pace frames ourselves with wl_surface.frame; never block in eglSwapBuffers.
        self.egl.swap_interval(self.display, 0)?;
        Ok(surface)
    }

    /// Must be called before the surface's `WlEglSurface` is dropped.
    pub fn destroy_surface(&self, surface: egl::Surface) {
        let _ = self
            .egl
            .make_current(self.display, None, None, Some(self.context));
        let _ = self.egl.destroy_surface(self.display, surface);
    }

    /// Renders one frame into `surface` and swaps (which commits the wl_surface).
    pub fn draw(&mut self, surface: egl::Surface, frame: &Frame) -> Result<(), Box<dyn Error>> {
        self.egl.make_current(
            self.display,
            Some(surface),
            Some(surface),
            Some(self.context),
        )?;

        let cell_h = self.cell_size * frame.scale;
        let cell_w = cell_h * self.cell_aspect;
        let cols = (frame.width as f32 / cell_w).ceil() as i32;
        let rows = (frame.height as f32 / cell_h).ceil() as i32;
        self.reserve_cells(cols, rows)?;
        let lod = (self.atlas_cell_height / cell_h).log2().max(0.0);
        let clock = frame
            .clock
            .as_ref()
            .map(|c| (c, self.clock_layout(frame, cell_w, cell_h)));
        let p = &self.pipeline;
        // SAFETY: plain GL calls on the current context; the VAO and textures stay bound.
        unsafe {
            let gl = &self.gl;

            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(p.cells_fbo));
            gl.viewport(0, 0, cols, rows);
            gl.use_program(Some(p.cells_program));
            let u = &p.cells_uniforms;
            gl.uniform_1_f32(u.time.as_ref(), frame.time);
            gl.uniform_1_f32(u.rows.as_ref(), rows as f32);
            gl.uniform_1_u32(u.seed.as_ref(), frame.seed);
            if let Some((clock, ([x, y, w, h], clock_lod))) = clock {
                gl.uniform_4_f32(u.clock_rect.as_ref(), x, y, w, h);
                let [d0, d1, d2, d3] = clock.digits;
                gl.uniform_4_u32(u.clock_digits.as_ref(), d0, d1, d2, d3);
                gl.uniform_1_f32(u.clock_colon.as_ref(), f32::from(u8::from(clock.colon)));
                gl.uniform_1_f32(u.clock_lod.as_ref(), clock_lod);
            } else {
                // A zero-size rect turns the clock off in the shader.
                gl.uniform_4_f32(u.clock_rect.as_ref(), 0.0, 0.0, 0.0, 0.0);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);

            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, frame.width, frame.height);
            gl.use_program(Some(p.rain_program));
            let u = &p.rain_uniforms;
            gl.uniform_2_f32(
                u.resolution.as_ref(),
                frame.width as f32,
                frame.height as f32,
            );
            gl.uniform_2_f32(u.cell.as_ref(), cell_w, cell_h);
            gl.uniform_1_f32(u.lod.as_ref(), lod);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
        self.egl.swap_buffers(self.display, surface)?;
        Ok(())
    }

    /// The clock's rect in cell units (x, y, width, height; rows from the top), centred on the
    /// output, and the clock atlas mip level that averages one cell's footprint.
    fn clock_layout(&self, frame: &Frame, cell_w: f32, cell_h: f32) -> ([f32; 4], f32) {
        let (width, height) = (frame.width as f32, frame.height as f32);
        // Clock width divided by digit height.
        let slots = (4.0 + CLOCK_COLON_WIDTH) * self.clock_aspect;
        // Shrink to fit narrow (e.g. portrait) outputs.
        let digit_h = (self.clock_size * height).min(CLOCK_MAX_WIDTH * width / slots);
        let clock_w = slots * digit_h;
        let x = (width - clock_w) / 2.0;
        let y = (height - digit_h) / 2.0;
        let rect = [x / cell_w, y / cell_h, clock_w / cell_w, digit_h / cell_h];
        let lod = ((cell_w * cell_h).sqrt() * CLOCK_ATLAS_HEIGHT as f32 / digit_h)
            .log2()
            .max(0.0);
        (rect, lod)
    }

    /// Grows the cell-state texture to hold at least `cols` x `rows` cells.
    fn reserve_cells(&mut self, cols: i32, rows: i32) -> Result<(), String> {
        let (have_cols, have_rows) = self.cells_capacity;
        if cols <= have_cols && rows <= have_rows {
            return Ok(());
        }
        let size = (cols.max(have_cols), rows.max(have_rows));
        // SAFETY: plain GL calls on the current context.
        unsafe {
            let gl = &self.gl;
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.pipeline.cells_texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                size.0,
                size.1,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.pipeline.cells_fbo));
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                return Err(format!("cell framebuffer incomplete: {status:#x}"));
            }
        }
        self.cells_capacity = size;
        Ok(())
    }
}

unsafe fn compile(gl: &glow::Context, kind: u32, src: &str) -> Result<glow::Shader, String> {
    let shader = gl.create_shader(kind)?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        return Err(format!(
            "shader compile failed: {}",
            gl.get_shader_info_log(shader)
        ));
    }
    Ok(shader)
}

unsafe fn link(
    gl: &glow::Context,
    vs: glow::Shader,
    fs_src: &str,
) -> Result<glow::Program, String> {
    let fs = compile(gl, glow::FRAGMENT_SHADER, fs_src)?;
    let program = gl.create_program()?;
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.link_program(program);
    gl.delete_shader(fs);
    if !gl.get_program_link_status(program) {
        return Err(format!(
            "program link failed: {}",
            gl.get_program_info_log(program)
        ));
    }
    Ok(program)
}

unsafe fn setup_gl(
    gl: &glow::Context,
    atlas: &atlas::Atlas,
    clock: Option<&atlas::ClockAtlas>,
    args: &Args,
) -> Result<Pipeline, String> {
    let vs = compile(gl, glow::VERTEX_SHADER, VERTEX_SHADER)?;
    let cells_program = link(gl, vs, CELLS_SHADER)?;
    let rain_program = link(gl, vs, RAIN_SHADER)?;
    gl.delete_shader(vs);

    // Full-screen triangle generated from gl_VertexID; no vertex buffers.
    let vao = gl.create_vertex_array()?;
    gl.bind_vertex_array(Some(vao));

    // Unit 0: glyph atlas.
    let atlas_texture = gl.create_texture()?;
    gl.active_texture(glow::TEXTURE0);
    gl.bind_texture(glow::TEXTURE_2D, Some(atlas_texture));
    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::R8 as i32,
        atlas.width as i32,
        atlas.height as i32,
        0,
        glow::RED,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(Some(&atlas.pixels)),
    );
    gl.generate_mipmap(glow::TEXTURE_2D);
    set_filter(gl, glow::LINEAR_MIPMAP_LINEAR, glow::LINEAR);

    // Unit 1: cell state, rendered by pass 1. Storage is allocated by `reserve_cells`.
    let cells_texture = gl.create_texture()?;
    gl.active_texture(glow::TEXTURE1);
    gl.bind_texture(glow::TEXTURE_2D, Some(cells_texture));
    set_filter(gl, glow::NEAREST, glow::NEAREST);
    let cells_fbo = gl.create_framebuffer()?;
    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(cells_fbo));
    gl.framebuffer_texture_2d(
        glow::FRAMEBUFFER,
        glow::COLOR_ATTACHMENT0,
        glow::TEXTURE_2D,
        Some(cells_texture),
        0,
    );
    gl.bind_framebuffer(glow::FRAMEBUFFER, None);

    // Unit 2: clock digits (only with --clock).
    if let Some(clock) = clock {
        let clock_texture = gl.create_texture()?;
        gl.active_texture(glow::TEXTURE2);
        gl.bind_texture(glow::TEXTURE_2D, Some(clock_texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::R8 as i32,
            clock.width as i32,
            clock.height as i32,
            0,
            glow::RED,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&clock.pixels)),
        );
        gl.generate_mipmap(glow::TEXTURE_2D);
        set_filter(gl, glow::LINEAR_MIPMAP_LINEAR, glow::LINEAR);
    }

    let glyph_count = atlas.glyph_count as f32;
    let [r, g, b] = args.color;

    gl.use_program(Some(cells_program));
    let loc = |name: &str| gl.get_uniform_location(cells_program, name);
    gl.uniform_1_f32(loc("u_glyph_count").as_ref(), glyph_count);
    gl.uniform_1_f32(loc("u_speed").as_ref(), args.speed);
    gl.uniform_1_i32(loc("u_clock").as_ref(), 2);
    gl.uniform_1_f32(loc("u_clock_colon_width").as_ref(), CLOCK_COLON_WIDTH);
    let cells_uniforms = CellsUniforms {
        time: loc("u_time"),
        rows: loc("u_rows"),
        seed: loc("u_seed"),
        clock_rect: loc("u_clock_rect"),
        clock_digits: loc("u_clock_digits"),
        clock_colon: loc("u_clock_colon"),
        clock_lod: loc("u_clock_lod"),
    };

    gl.use_program(Some(rain_program));
    let loc = |name: &str| gl.get_uniform_location(rain_program, name);
    gl.uniform_1_i32(loc("u_atlas").as_ref(), 0);
    gl.uniform_1_i32(loc("u_cells").as_ref(), 1);
    gl.uniform_1_f32(loc("u_glyph_count").as_ref(), glyph_count);
    gl.uniform_3_f32(loc("u_color").as_ref(), r, g, b);
    let rain_uniforms = RainUniforms {
        resolution: loc("u_resolution"),
        cell: loc("u_cell"),
        lod: loc("u_lod"),
    };

    Ok(Pipeline {
        cells_program,
        cells_uniforms,
        rain_program,
        rain_uniforms,
        cells_fbo,
        cells_texture,
    })
}

/// Sets filtering (and edge clamping) on the bound 2D texture.
unsafe fn set_filter(gl: &glow::Context, min: u32, mag: u32) {
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, min as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, mag as i32);
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
}
