// Presentation shader for the 64-bit host.
//
// Hard constraints learned from the D3DMetal/FXC path:
//
//  * No `sampler` binding. Naga's HLSL backend turns a sampler into a
//    sampler-heap/resource array, which D3DMetal's FXC rejects with
//    "E5017: Shader model 5.1+ resource array". The original in-process
//    renderer also used `textureLoad`, never `textureSample`.
//  * No `textureDimensions()`. The size is passed in a uniform instead, so the
//    shader does not depend on `GetDimensions` translation.
//  * The vertex stage returns `@builtin(position)` directly, never a struct
//    with a `@location` varying. `solid.wgsl` draws fine with exactly this
//    shape while the struct-returning version produced a silently dropped
//    draw, even with an empty pipeline layout and a constant-colour fragment
//    shader - so the varying, not the bind group, was what D3DMetal refused.
//
// The frame texel is derived from `@builtin(position)`, which is a plain
// `SV_Position` fragment input: no interpolation is involved at all.

struct Params {
    tex_size: vec2<f32>,
    origin: vec2<f32>,
    target_size: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var raw_frame: texture_2d<f32>;
@group(0) @binding(1) var<uniform> params: Params;

// Single oversized triangle covering the whole clip volume. Identical to the
// vertex stage in `solid.wgsl`, which is the one variant known to rasterise.
@vertex
fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}

// Default path: map the destination framebuffer pixel through the viewport.
// Magenta outside the viewport makes a wrong origin/scale immediately visible.
@fragment
fn fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let local = (position.xy - params.origin) / params.target_size;
    if local.x < 0.0 || local.y < 0.0 || local.x >= 1.0 || local.y >= 1.0 {
        return vec4<f32>(1.0, 0.0, 1.0, 1.0);
    }
    let size = vec2<i32>(params.tex_size);
    let pixel = clamp(vec2<i32>(local * params.tex_size), vec2<i32>(0), size - vec2<i32>(1));
    return textureLoad(raw_frame, pixel, 0);
}

// Diagnostic: proves the pipeline + bind group + rasterisation work without
// reading the texture at all. Green means "draw happened"; black means the
// draw itself (not the texture) is the problem.
@fragment
fn fs_const() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 1.0, 0.0, 1.0);
}
