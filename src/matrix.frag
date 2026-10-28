#version 300 es
// Pass 2: full resolution. Looks up this pixel's cell state (from cells.frag) and draws the glyph.
precision highp float;
precision highp int;

uniform vec2 u_resolution;   // buffer size, physical px
uniform vec2 u_cell;         // cell size, physical px
uniform float u_lod;         // atlas mip level matching u_cell
uniform sampler2D u_atlas;   // R8, glyphs side by side (pre-mirrored)
uniform sampler2D u_cells;   // RGBA8 cell state, one texel per cell
uniform float u_glyph_count;
uniform vec3 u_color;

out vec4 frag_color;

void main() {
    vec2 p = vec2(gl_FragCoord.x, u_resolution.y - gl_FragCoord.y); // top-down
    vec2 g = p / u_cell;
    vec2 cell = floor(g);
    vec2 local = g - cell;
    vec4 state = texelFetch(u_cells, ivec2(cell), 0);
    float intensity = state.r;
    float head = state.g;

    vec3 c = u_color * 0.012; // faint dark-green base
    if (intensity > 0.0) {
        float glyph = floor(state.b * 255.0 + 0.5);
        vec2 uv = vec2((glyph + local.x) / u_glyph_count, local.y);
        float a = textureLod(u_atlas, uv, u_lod).r;

        vec3 rain = mix(u_color, vec3(1.0), head * 0.85);
        c += rain * a * intensity * state.a;

        // Soft haze along the trail and a glow around the head.
        vec2 q = local - 0.5;
        c += u_color * (intensity * 0.025 + head * 0.12 * exp(-8.0 * dot(q, q)));
    }

    frag_color = vec4(c, 1.0);
}
