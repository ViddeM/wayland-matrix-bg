//! Rasterizes the rain glyphs, and the clock digits, into single-row, single-channel atlases.

use std::error::Error;
use std::path::PathBuf;
use std::process::Command;

use ab_glyph::{point, Font, FontVec, GlyphId, OutlinedGlyph, PxScale};

/// Glyphs that set the cell size (others are centred and clipped to fit).
const KATAKANA: &str = "ｦｧｨｩｪｫｬｭｮｯｰｱｲｳｴｵｶｷｸｹｺｻｼｽｾｿﾀﾁﾂﾃﾄﾅﾆﾇﾈﾉﾊﾋﾌﾍﾎﾏﾐﾑﾒﾓﾔﾕﾖﾗﾘﾙﾚﾛﾜﾝ";
const DIGITS: &str = "0123456789";
const SYMBOLS: &str = ":.\"=*+-<>¦|";

/// Fraction of the cell height covered by the tallest primary glyphs.
const GLYPH_FILL: f32 = 0.80;
/// Horizontal gap between columns, as a fraction of the cell height.
const COLUMN_GAP: f32 = 0.16;

/// Clock atlas order: the digits, then the colon (index 10).
const CLOCK_GLYPHS: &str = "0123456789:";
/// Fraction of the clock cell height covered by the digits (a sliver of margin for rounding).
const CLOCK_FILL: f32 = 0.98;
/// Gap between clock digits, as a fraction of the digit height.
const CLOCK_GAP: f32 = 0.2;

pub struct Atlas {
    /// R8 pixels, `width * height`, glyphs left to right.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub glyph_count: u32,
    /// Cell width divided by cell height.
    pub cell_aspect: f32,
}

pub struct ClockAtlas {
    /// R8 pixels, `width * height`, in `CLOCK_GLYPHS` order, unmirrored.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Digit cell width divided by digit height.
    pub aspect: f32,
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

/// Loads the `--font` face (or fc-match's default), with its path for messages.
pub fn load_font(spec: Option<&str>) -> Result<(FontVec, String), Box<dyn Error>> {
    let (path, index) = find_font(spec)?;
    let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let font = FontVec::try_from_vec_and_index(data, index)
        .map_err(|e| format!("{}:{index}: {e}", path.display()))?;
    Ok((font, path.display().to_string()))
}

/// Builds the rain atlas with cells `cell_height` pixels tall.
pub fn build(font: &FontVec, label: &str, cell_height: u32) -> Result<Atlas, Box<dyn Error>> {
    let ids = |s: &'static str| {
        s.chars()
            .map(|c| font.glyph_id(c))
            .filter(|id| *id != GlyphId(0))
            .collect::<Vec<_>>()
    };
    let katakana = ids(KATAKANA);
    if katakana.len() < KATAKANA.chars().count() / 2 {
        return Err(format!(
            "{label} lacks half-width katakana; install a CJK font (e.g. Noto Sans CJK) or pass --font"
        )
        .into());
    }
    let primary: Vec<GlyphId> = katakana.into_iter().chain(ids(DIGITS)).collect();
    let symbols = ids(SYMBOLS);

    let h = cell_height as f32;
    let (scale, baseline, max_w) = fit(font, &primary, h, GLYPH_FILL);
    let cell = ((max_w + COLUMN_GAP * h).ceil() as u32, cell_height);

    let glyphs: Vec<GlyphId> = primary.into_iter().chain(symbols).collect();
    let width = cell.0 * glyphs.len() as u32;
    let mut pixels = vec![0u8; (width * cell_height) as usize];
    for (i, &id) in glyphs.iter().enumerate() {
        if let Some(g) = font.outline_glyph(id.with_scale_and_position(scale, point(0.0, baseline)))
        {
            // Mirrored horizontally, like the film.
            blit(&mut pixels, width, cell, i as u32, &g, true);
        }
    }

    Ok(Atlas {
        pixels,
        width,
        height: cell_height,
        glyph_count: glyphs.len() as u32,
        cell_aspect: cell.0 as f32 / h,
    })
}

/// Builds the clock atlas: digits scaled to fill `height` pixels, the colon on their baseline.
pub fn build_clock(font: &FontVec, height: u32) -> Result<ClockAtlas, Box<dyn Error>> {
    let glyphs: Vec<GlyphId> = CLOCK_GLYPHS.chars().map(|c| font.glyph_id(c)).collect();
    if glyphs.contains(&GlyphId(0)) {
        return Err("font lacks the digits or ':' needed by --clock".into());
    }

    let h = height as f32;
    let (scale, baseline, max_w) = fit(font, &glyphs[..10], h, CLOCK_FILL);
    let cell = ((max_w + CLOCK_GAP * h).ceil() as u32, height);
    let width = cell.0 * glyphs.len() as u32;
    let mut pixels = vec![0u8; (width * height) as usize];
    for (i, &id) in glyphs.iter().enumerate() {
        if let Some(g) = font.outline_glyph(id.with_scale_and_position(scale, point(0.0, baseline)))
        {
            blit(&mut pixels, width, cell, i as u32, &g, false);
        }
    }

    Ok(ClockAtlas {
        pixels,
        width,
        height,
        aspect: cell.0 as f32 / h,
    })
}

/// Scale and baseline that make the ink of `ids` span `fill` of a `height`-pixel cell, centred
/// vertically, plus the widest glyph's ink width at that scale.
fn fit(font: &FontVec, ids: &[GlyphId], height: f32, fill: f32) -> (PxScale, f32, f32) {
    // Measure at a reference scale; everything scales linearly.
    let reference = PxScale::from(100.0);
    let (mut top, mut bottom, mut max_w) = (f32::MAX, f32::MIN, 0f32);
    for &id in ids {
        if let Some(g) = font.outline_glyph(id.with_scale_and_position(reference, point(0.0, 0.0)))
        {
            let b = g.px_bounds();
            top = top.min(b.min.y);
            bottom = bottom.max(b.max.y);
            max_w = max_w.max(b.width());
        }
    }
    let k = fill * height / (bottom - top);
    let baseline = (height - (bottom - top) * k) / 2.0 - top * k;
    (PxScale::from(100.0 * k), baseline, max_w * k)
}

/// Draws `g` into cell `index` of a single-row atlas `width` pixels wide, centred horizontally
/// and clipped to the cell; `cell` is (width, height).
fn blit(
    pixels: &mut [u8],
    width: u32,
    cell: (u32, u32),
    index: u32,
    g: &OutlinedGlyph,
    mirror: bool,
) {
    let (cell_w, cell_h) = cell;
    let b = g.px_bounds();
    let x_off = ((cell_w as f32 - b.width()) / 2.0).round() as i32;
    let y_off = b.min.y as i32;
    let x0 = index * cell_w;
    g.draw(|x, y, c| {
        let col = x_off + x as i32;
        let row = y_off + y as i32;
        if col < 0 || col >= cell_w as i32 || row < 0 || row >= cell_h as i32 {
            return;
        }
        let col = if mirror {
            cell_w - 1 - col as u32
        } else {
            col as u32
        };
        let p = &mut pixels[(row as u32 * width + x0 + col) as usize];
        *p = (*p).max((c.clamp(0.0, 1.0) * 255.0).round() as u8);
    });
}
