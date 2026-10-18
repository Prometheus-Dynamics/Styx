@group(0) @binding(0)
var input_tex: texture_2d<f32>;

@group(0) @binding(1)
var output_tex: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(output_tex);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }

    let xy = vec2<u32>(id.xy);
    let pixel = textureLoad(input_tex, xy, 0);
    let centered = pixel.rgb - vec3<f32>(0.5);
    let boosted = clamp(centered * 1.35 + vec3<f32>(0.5), vec3<f32>(0.0), vec3<f32>(1.0));
    textureStore(output_tex, xy, vec4<f32>(boosted, pixel.a));
}
