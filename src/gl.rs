//! EGL/GLES setup shared by all outputs and the rain draw call.

use std::error::Error;
use std::ffi::c_void;

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
}

struct CellsUniforms {
    time: Option<glow::UniformLocation>,
    rows: Option<glow::UniformLocation>,
    seed: Option<glow::UniformLocation>,
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
        let atlas = atlas::build(args.font.as_deref(), atlas_cell_height)?;

        // SAFETY: plain GL calls on the current context.
        let pipeline = unsafe { setup_gl(&gl, &atlas, args)? };

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

    let glyph_count = atlas.glyph_count as f32;
    let [r, g, b] = args.color;

    gl.use_program(Some(cells_program));
    let loc = |name: &str| gl.get_uniform_location(cells_program, name);
    gl.uniform_1_f32(loc("u_glyph_count").as_ref(), glyph_count);
    gl.uniform_1_f32(loc("u_speed").as_ref(), args.speed);
    let cells_uniforms = CellsUniforms {
        time: loc("u_time"),
        rows: loc("u_rows"),
        seed: loc("u_seed"),
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
