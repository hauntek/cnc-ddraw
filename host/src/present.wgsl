struct Vertex {
    @builtin(position) position: vec4<f32>,
};

struct Uniforms {
    // Pixel offset and 1/size of the destination viewport.
    offset: vec2<f32>,
    scale: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) index: u32) -> Vertex {
    // Fullscreen triangle. DirectDraw scanlines start at the top.
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return Vertex(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0));
}

@group(0) @binding(0) var raw_frame: texture_2d<f32>;
@group(0) @binding(1) var<uniform> viewport: Uniforms;

@fragment
fn fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    // Derive UV from the framebuffer position instead of interpolating a
    // vertex output. D3DMetal's translation of interpolated @location inputs
    // is one of the remaining suspects for the all-black draw.
    let uv = (position.xy - viewport.offset) * viewport.scale;
    let size = vec2<i32>(textureDimensions(raw_frame));
    let pixel = clamp(vec2<i32>(uv * vec2<f32>(size)), vec2<i32>(0), size - vec2<i32>(1));
    return textureLoad(raw_frame, pixel, 0);
}
