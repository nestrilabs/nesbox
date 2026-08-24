#version 450
// Fullscreen triangle, no vertex buffers. Standard trick: three vertices whose
// clip coords cover the whole viewport, derived from gl_VertexIndex.
void main() {
    vec2 p = vec2(float((gl_VertexIndex << 1) & 2), float(gl_VertexIndex & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
