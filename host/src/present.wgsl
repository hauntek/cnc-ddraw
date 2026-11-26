struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) index: u32) -> Vertex {
    // Same fullscreen triangle as the original in-process renderer.
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return Vertex(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}

@group(0) @binding(0) var raw_frame: texture_2d<f32>;

@fragment
fn fs(input: Vertex) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(raw_frame));
    let pixel = clamp(vec2<i32>(input.uv * vec2<f32>(size)), vec2<i32>(0), size - vec2<i32>(1));
    return textureLoad(raw_frame, pixel, 0);
}
