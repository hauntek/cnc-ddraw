// Integer-texture presentation.
//
// The host uploads the raw DirectDraw surface bytes (indexed / RGB555 / RGB565 /
// XRGB) and the GPU does the format unpack, the palette or gamma LUT lookup and
// the nearest-neighbour texel selection. The CPU never touches a pixel, which is
// how the original in-process renderer worked.
//
// Same hard constraints as `present.wgsl`: no sampler binding (D3DMetal's FXC
// rejects the sampler-heap resource array with E5017), no
// `textureDimensions()`, and the vertex stage returns `@builtin(position)`
// directly instead of a struct with a varying.

struct Params {
    tex_size: vec2<f32>,
    origin: vec2<f32>,
    target_size: vec2<f32>,
    // 0 = Indexed, 1 = RGB555, 2 = RGB565, 3 = XRGB8888.
    format: u32,
    _pad: u32,
};

@group(0) @binding(0) var raw_frame: texture_2d<u32>;
@group(0) @binding(1) var<uniform> params: Params;
@group(0) @binding(2) var lut: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}

fn lut_rgb(r: u32, g: u32, b: u32) -> vec4<f32> {
    return vec4<f32>(
        textureLoad(lut, vec2<i32>(i32(r), 0), 0).r,
        textureLoad(lut, vec2<i32>(i32(g), 0), 0).g,
        textureLoad(lut, vec2<i32>(i32(b), 0), 0).b,
        1.0,
    );
}

@fragment
fn fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let local = (position.xy - params.origin) / params.target_size;
    if local.x < 0.0 || local.y < 0.0 || local.x >= 1.0 || local.y >= 1.0 {
        return vec4<f32>(1.0, 0.0, 1.0, 1.0);
    }
    let size = vec2<i32>(params.tex_size);
    let pixel = clamp(vec2<i32>(local * params.tex_size), vec2<i32>(0), size - vec2<i32>(1));
    let raw = textureLoad(raw_frame, pixel, 0).r;

    if params.format == 0u {
        // 8-bit indexed: the palette entry already holds the final colour.
        return textureLoad(lut, vec2<i32>(i32(raw & 0xffu), 0), 0);
    }
    if params.format == 1u {
        // RGB555
        return lut_rgb(
            (((raw >> 10u) & 31u) * 255u) / 31u,
            (((raw >> 5u) & 31u) * 255u) / 31u,
            ((raw & 31u) * 255u) / 31u,
        );
    }
    if params.format == 2u {
        // RGB565
        return lut_rgb(
            (((raw >> 11u) & 31u) * 255u) / 31u,
            (((raw >> 5u) & 63u) * 255u) / 63u,
            ((raw & 31u) * 255u) / 31u,
        );
    }
    // XRGB8888: little-endian BGRX in memory reads as 0xXXRRGGBB.
    return lut_rgb((raw >> 16u) & 0xffu, (raw >> 8u) & 0xffu, raw & 0xffu);
}

// Diagnostic: proves the pipeline and rasterisation work without reading the
// frame texture at all.
@fragment
fn fs_const() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 1.0, 0.0, 1.0);
}
