// NV12 -> RGB for `decode::render`. Two textures: Y (R8Unorm, w x h) and
// interleaved UV (Rg8Unorm, ceil(w/2) x ceil(h/2)). The YUV->RGB matrix,
// including the limited/full range expansion, comes in as three affine rows
// computed on the CPU (`render::yuv_to_rgb_rows`), so this shader has no
// per-standard branches.

struct Params {
    // rgb = (dot(r, v), dot(g, v), dot(b, v)) with v = (y, u, v, 1)
    r: vec4<f32>,
    g: vec4<f32>,
    b: vec4<f32>,
    // xy: scale from luma texture coords to chroma texture coords (1.0 for
    //     even sizes; w / (2 * ceil(w/2)) for odd ones).
    // z:  1.0 if the render target is an *Srgb format (output must be linear).
    extra: vec4<f32>,
};

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) tc: vec2<f32>,
};

// Triangle strip over the whole viewport (which egui / the callback set to
// the video rect): vertex i -> corner (i & 1, i >> 1).
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let x = f32(i & 1u);
    let y = f32((i >> 1u) & 1u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.tc = vec2<f32>(x, y);
    return out;
}

fn linear_from_srgb(c: vec3<f32>) -> vec3<f32> {
    let cutoff = c < vec3<f32>(0.04045);
    let lower = c / vec3<f32>(12.92);
    let higher = pow((c + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let luma = textureSample(y_tex, samp, in.tc).r;
    let chroma = textureSample(uv_tex, samp, in.tc * params.extra.xy).rg;
    let v = vec4<f32>(luma, chroma.x, chroma.y, 1.0);
    var rgb = clamp(vec3<f32>(dot(params.r, v), dot(params.g, v), dot(params.b, v)), vec3<f32>(0.0), vec3<f32>(1.0));
    if params.extra.z > 0.5 {
        rgb = linear_from_srgb(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
