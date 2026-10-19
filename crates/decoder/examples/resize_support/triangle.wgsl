struct Dims { sw: u32, sh: u32, dw: u32, dh: u32 }
@group(0) @binding(0) var<storage, read> source: array<u32>;
@group(0) @binding(1) var<storage, read_write> intermediate: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> destination: array<u32>;
@group(0) @binding(3) var<uniform> d: Dims;

@compute @workgroup_size(8, 8)
fn horizontal(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= d.dw || id.y >= d.sh { return; }
    let scale = f32(d.sw) / f32(d.dw);
    let radius = max(scale, 1.0);
    let center = (f32(id.x) + 0.5) * scale - 0.5;
    let lo = max(i32(ceil(center - radius)), 0);
    let hi = min(i32(floor(center + radius)), i32(d.sw) - 1);
    var sum = vec4<f32>(0.0);
    var weights = 0.0;
    for (var x = lo; x <= hi; x++) {
        let weight = max(0.0, 1.0 - abs(f32(x) - center) / radius);
        let p = unpack4x8unorm(source[id.y * d.sw + u32(x)]);
        sum += vec4<f32>(p.rgb * p.a, p.a) * weight;
        weights += weight;
    }
    intermediate[id.y * d.dw + id.x] = sum / weights;
}

@compute @workgroup_size(8, 8)
fn vertical(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= d.dw || id.y >= d.dh { return; }
    let scale = f32(d.sh) / f32(d.dh);
    let radius = max(scale, 1.0);
    let center = (f32(id.y) + 0.5) * scale - 0.5;
    let lo = max(i32(ceil(center - radius)), 0);
    let hi = min(i32(floor(center + radius)), i32(d.sh) - 1);
    var sum = vec4<f32>(0.0);
    var weights = 0.0;
    for (var y = lo; y <= hi; y++) {
        let weight = max(0.0, 1.0 - abs(f32(y) - center) / radius);
        sum += intermediate[u32(y) * d.dw + id.x] * weight;
        weights += weight;
    }
    let p = sum / weights;
    var rgb = vec3<f32>(0.0);
    if p.a > 0.000001 { rgb = p.rgb / p.a; }
    destination[id.y * d.dw + id.x] = pack4x8unorm(vec4<f32>(rgb, p.a));
}
