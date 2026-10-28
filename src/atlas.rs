//! Rasterizes the rain glyphs into a single-row, single-channel atlas.

use std::error::Error;
use std::path::PathBuf;
use std::process::Command;

use ab_glyph::{point, Font, FontVec, GlyphId, PxScale};

/// Glyphs that set the cell size (others are centred and clipped to fit).
const KATAKANA: &str = "ｦｧｨｩｪｫｬｭｮｯｰｱｲｳｴｵｶｷｸｹｺｻｼｽｾｿﾀﾁﾂﾃﾄﾅﾆﾇﾈﾉﾊﾋﾌﾍﾎﾏﾐﾑﾒﾓﾔﾕﾖﾗﾘﾙﾚﾛﾜﾝ";
const DIGITS: &str = "0123456789";
const SYMBOLS: &str = ":.\"=*+-<>¦|";

/// Fraction of the cell height covered by the tallest primary glyphs.
const GLYPH_FILL: f32 = 0.80;
/// Horizontal gap between columns, as a fraction of the cell height.
const COLUMN_GAP: f32 = 0.16;

pub struct Atlas {
    /// R8 pixels, `width * height`, glyphs left to right.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub glyph_count: u32,
    /// Cell width divided by cell height.
    pub cell_aspect: f32,
}

pub fn find_font(spec: Option<&str>) -> Result<(PathBuf, u32), Box<dyn Error>> {
    if let Some(spec) = spec {
        return Ok(crate::parse_font_spec(spec));
    }
    let out = Command::new("fc-match")
        .args(["-f", "%{file}:%{index}", "Noto Sans Mono CJK JP"])
        .output()
        .map_err(|e| format!("failed to run fc-match (use --font): {e}"))?;
    let spec = String::from_utf8(out.stdout)?;
    if !out.status.success() || spec.trim().is_empty() {
        return Err("fc-match found no font (use --font)".into());
    }
    Ok(crate::parse_font_spec(spec.trim()))
}

/// Builds the atlas with cells `cell_height` pixels tall.
pub fn build(font_spec: Option<&str>, cell_height: u32) -> Result<Atlas, Box<dyn Error>> {
    let (path, index) = find_font(font_spec)?;
    let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let font = FontVec::try_from_vec_and_index(data, index)
        .map_err(|e| format!("{}:{index}: {e}", path.display()))?;

    let ids = |s: &'static str| {
        s.chars()
            .map(|c| font.glyph_id(c))
            .filter(|id| *id != GlyphId(0))
            .collect::<Vec<_>>()
    };
    let katakana = ids(KATAKANA);
    if katakana.len() < KATAKANA.chars().count() / 2 {
        return Err(format!(
            "{} lacks half-width katakana; install a CJK font (e.g. Noto Sans CJK) or pass --font",
            path.display()
        )
        .into());
    }
    let primary: Vec<GlyphId> = katakana.into_iter().chain(ids(DIGITS)).collect();
    let symbols = ids(SYMBOLS);

    // Measure the primary glyphs at a reference scale; everything scales linearly.
    let reference = PxScale::from(100.0);
    let (mut top, mut bottom, mut max_w) = (f32::MAX, f32::MIN, 0f32);
    for &id in &primary {
        if let Some(g) = font.outline_glyph(id.with_scale_and_position(reference, point(0.0, 0.0)))
        {
            let b = g.px_bounds();
            top = top.min(b.min.y);
            bottom = bottom.max(b.max.y);
            max_w = max_w.max(b.width());
        }
    }
    let h = cell_height as f32;
    let k = GLYPH_FILL * h / (bottom - top);
    let scale = PxScale::from(100.0 * k);
    let baseline = (h - (bottom - top) * k) / 2.0 - top * k;
    let cell_w = (max_w * k + COLUMN_GAP * h).ceil() as u32;

    let glyphs: Vec<GlyphId> = primary.into_iter().chain(symbols).collect();
    let width = cell_w * glyphs.len() as u32;
    let mut pixels = vec![0u8; (width * cell_height) as usize];

    for (i, &id) in glyphs.iter().enumerate() {
        let Some(g) = font.outline_glyph(id.with_scale_and_position(scale, point(0.0, baseline)))
        else {
            continue;
        };
        let b = g.px_bounds();
        let x_off = ((cell_w as f32 - b.width()) / 2.0).round() as i32;
        let y_off = b.min.y as i32;
        let x0 = i as u32 * cell_w;
        g.draw(|x, y, c| {
            let col = x_off + x as i32;
            let row = y_off + y as i32;
            if col < 0 || col >= cell_w as i32 || row < 0 || row >= cell_height as i32 {
                return;
            }
            // Mirror horizontally, like the film.
            let col = cell_w - 1 - col as u32;
            let p = &mut pixels[(row as u32 * width + x0 + col) as usize];
            *p = (*p).max((c.clamp(0.0, 1.0) * 255.0).round() as u8);
        });
    }

    Ok(Atlas {
        pixels,
        width,
        height: cell_height,
        glyph_count: glyphs.len() as u32,
        cell_aspect: cell_w as f32 / h,
    })
}
