struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) index: u32) -> Vertex {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return Vertex(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}

@group(0) @binding(0) var raw_frame: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;

@fragment
fn fs(input: Vertex) -> @location(0) vec4<f32> {
    // Use a normal sampled texture instead of textureLoad. textureLoad on an
    // UNORM texture is the one remaining D3D12/D3DMetal-specific difference
    // between this host and a conventional sampled-texture game.
    return textureSample(raw_frame, frame_sampler, input.uv);
}
