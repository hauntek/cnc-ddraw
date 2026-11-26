struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Params {
    // 0 = Indexed (R8Uint), 1 = RGB555 (R16Uint), 2 = RGB565 (R16Uint), 3 = XRGB8888 (R32Uint)
    format: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@vertex
fn vs(@builtin(vertex_index) index: u32) -> Vertex {
    // A single triangle spans the viewport. DirectDraw scanlines start at the top.
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return Vertex(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}

@group(0) @binding(0) var raw_frame: texture_2d<u32>;
@group(0) @binding(1) var lut: texture_2d<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@fragment
fn fs(input: Vertex) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(raw_frame));
    let pixel = clamp(vec2<i32>(input.uv * vec2<f32>(size)), vec2<i32>(0), size - vec2<i32>(1));
    let raw = textureLoad(raw_frame, pixel, 0).r;

    if params.format == 0u {
        // 8-bit indexed: palette (with optional gamma pre-baked) lives in `lut`.
        return textureLoad(lut, vec2<i32>(i32(raw & 0xffu), 0), 0);
    }

    var r: u32;
    var g: u32;
    var b: u32;
    if params.format == 1u {
        // RGB555
        r = (((raw >> 10u) & 31u) * 255u) / 31u;
        g = (((raw >> 5u) & 31u) * 255u) / 31u;
        b = ((raw & 31u) * 255u) / 31u;
    } else if params.format == 2u {
        // RGB565
        r = (((raw >> 11u) & 31u) * 255u) / 31u;
        g = (((raw >> 5u) & 63u) * 255u) / 63u;
        b = ((raw & 31u) * 255u) / 31u;
    } else {
        // XRGB8888 (little-endian BGRX in memory -> R32Uint = 0xXXRRGGBB)
        r = (raw >> 16u) & 0xffu;
        g = (raw >> 8u) & 0xffu;
        b = raw & 0xffu;
    }

    let r_out = textureLoad(lut, vec2<i32>(i32(r), 0), 0).r;
    let g_out = textureLoad(lut, vec2<i32>(i32(g), 0), 0).g;
    let b_out = textureLoad(lut, vec2<i32>(i32(b), 0), 0).b;
    return vec4<f32>(r_out, g_out, b_out, 1.0);
}
