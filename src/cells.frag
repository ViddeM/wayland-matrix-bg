#version 300 es
// Pass 1: one fragment per glyph cell. Stateless Matrix rain, derived from time and the
// cell coordinates; the result is packed into an RGBA8 texel for the full-resolution pass.
precision highp float;
precision highp int;

uniform float u_time;        // seconds
uniform float u_rows;        // grid rows on this output
uniform float u_glyph_count;
uniform float u_speed;
uniform uint u_seed;         // per output

uniform sampler2D u_clock;          // R8 clock glyphs 0-9 then ':', unmirrored
uniform vec4 u_clock_rect;          // x, y, w, h in cells (rows from the top); w = 0: no clock
uniform uvec4 u_clock_digits;       // H H M M
uniform float u_clock_colon;        // 1 = colon shown
uniform float u_clock_colon_width;  // colon slot width, in digit widths
uniform float u_clock_lod;          // u_clock mip level that averages one cell

// r: intensity, g: head (1 = head cell), b: glyph index / 255, a: brightness
out vec4 cell_state;

const int LANES = 3;
const float MAX_TRAIL = 35.0;
const float CLOCK_GLYPHS = 11.0;
const float MOAT = 1.25;     // cells around the clock strokes where the rain is dimmed

uvec3 pcg3d(uvec3 v) {
    v = v * 1664525u + 1013904223u;
    v.x += v.y * v.z; v.y += v.z * v.x; v.z += v.x * v.y;
    v ^= v >> 16u;
    v.x += v.y * v.z; v.y += v.z * v.x; v.z += v.x * v.y;
    return v;
}

// Three uniform randoms in [0, 1).
vec3 rand3(uint a, uint b, uint c) {
    return min(vec3(pcg3d(uvec3(a, b, c))) * (1.0 / 4294967296.0), 0.99999);
}

// Clock coverage at p (cell units, rows from the top); 0 outside the clock.
float clock_mask(vec2 p) {
    vec2 q = (p - u_clock_rect.xy) / u_clock_rect.zw;
    if (any(lessThan(q, vec2(0.0))) || any(greaterThanEqual(q, vec2(1.0)))) return 0.0;
    // Slots [d][d][:][d][d], x in digit widths.
    float x = q.x * (4.0 + u_clock_colon_width);
    float glyph;
    if (x >= 2.0 && x < 2.0 + u_clock_colon_width) {
        if (u_clock_colon == 0.0) return 0.0;
        glyph = 10.0;
        x += 0.5 * (1.0 - u_clock_colon_width) - 2.0; // centred part of the colon's cell
    } else {
        if (x >= 2.0) x -= u_clock_colon_width;
        float slot = min(floor(x), 3.0);
        glyph = float(u_clock_digits[int(slot)]);
        x -= slot;
    }
    return textureLod(u_clock, vec2((glyph + x) / CLOCK_GLYPHS, q.y), u_clock_lod).r;
}

void main() {
    // Texel (x, y) is cell (column, row), rows counted from the top.
    uint col = uint(gl_FragCoord.x);
    float row = floor(gl_FragCoord.y);
    float rows = u_rows;

    // Brightest drop covering this cell, and how close it is to that drop's head.
    float intensity = 0.0;
    float head = 0.0;
    for (int i = 0; i < LANES; i++) {
        uint lane = uint(i);
        vec3 r = rand3(col, lane, u_seed);
        if (i == LANES - 1 && r.x < 0.5) continue; // 2-3 lanes per column
        float speed = mix(6.0, 22.0, r.y) * u_speed; // cells per second
        float gap = rand3(col, lane + 8u, u_seed).x * rows;
        float cycle = rows + MAX_TRAIL + gap;
        float pos = u_time * speed + r.z * cycle;
        float pass = floor(pos / cycle);

        // Re-roll trail length and start row on every pass down the screen.
        vec3 rp = rand3(col * 4u + lane, uint(pass), u_seed ^ 0x5bd1e995u);
        float trail = mix(8.0, MAX_TRAIL, rp.x);
        float start = rp.y < 0.3 ? floor(rp.z * rows * 0.6) : 0.0;
        float head_row = floor(pos - pass * cycle) + start; // steps cell by cell

        float d = head_row - row;
        if (row >= start && d >= 0.0 && d < trail) {
            float v = pow(1.0 - d / trail, 1.5);
            if (v > intensity) {
                intensity = v;
                head = d < 0.5 ? 1.0 : (d < 1.5 ? 0.3 : 0.0);
            }
        }
    }

    // Clock: cells on the digit strokes stay lit, with a dark moat around them so the shape reads.
    if (u_clock_rect.z > 0.0) {
        vec2 p = gl_FragCoord.xy; // cell centre
        float m = clock_mask(p);
        float near = m;
        for (int dy = -1; dy <= 1; dy++) {
            for (int dx = -1; dx <= 1; dx++) {
                near = max(near, clock_mask(p + vec2(dx, dy) * MOAT));
            }
        }
        float stroke = smoothstep(0.25, 0.6, m);
        float dim = mix(1.0 - 0.8 * near, 1.0, stroke);
        intensity = max(intensity * dim, stroke * 0.9);
        head = max(head * dim, 0.15 * m);
    }

    float glyph = 0.0;
    float brightness = 0.0;
    if (intensity > 0.0) {
        uint urow = uint(row);
        vec3 rc = rand3(col, urow, u_seed ^ 0x27d4eb2fu);
        // Most glyphs change slowly, a few flicker fast, heads churn.
        float rate = rc.x < 0.1 ? mix(6.0, 20.0, rc.y) : mix(0.15, 1.2, rc.y);
        rate = mix(rate, 15.0, step(0.5, head));
        uint tick = uint(floor(u_time * rate + rc.z * 64.0));
        vec3 rg = rand3(col ^ (tick * 0x9e3779b9u), urow, u_seed);
        glyph = floor(rg.x * u_glyph_count);
        brightness = mix(0.7, 1.0, rg.y);
    }

    cell_state = vec4(intensity, head, glyph / 255.0, brightness);
}
