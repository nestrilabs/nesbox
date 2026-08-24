#version 450
// The cost knob.
//
// `cost` iterations of a *dependent* chain of transcendentals: each step needs
// the previous result, so no compiler can vectorise or hoist it, and nothing is
// dead-code eliminated because the accumulator reaches the output. GPU time per
// frame is therefore close to linear in `cost` x pixels, which is exactly the
// property a calibration probe needs and a real title cannot offer.
layout(push_constant) uniform Push { uint cost; } push;
layout(location = 0) out vec4 outColor;

void main() {
    float x = gl_FragCoord.x * 0.001;
    float y = gl_FragCoord.y * 0.001;
    float acc = 0.0;
    for (uint i = 0u; i < push.cost; ++i) {
        x = fract(sin(x * 12.9898 + y * 78.233) * 43758.5453);
        acc += x;
    }
    outColor = vec4(acc, x, y, 1.0);
}
