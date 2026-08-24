// Shaders are compiled ahead of time and committed as .spv, so a build needs no
// glslc. Recompile with:
//   glslc -O shaders/probe.vert -o shaders/probe.vert.spv
//   glslc -O shaders/probe.frag -o shaders/probe.frag.spv
fn main() {
    println!("cargo:rerun-if-changed=shaders/probe.vert.spv");
    println!("cargo:rerun-if-changed=shaders/probe.frag.spv");
}
