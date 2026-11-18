//! Animated shader background.
//! Optimizations:
//!  * the heavy shader (domain-warped fbm) renders into a texture at reduced
//!    resolution and is upscaled with bilinear filtering — the picture is soft anyway;
//!  * if neither time nor size changed (animation paused), the texture isn't
//!    re-rendered, only the cheap blit pass runs;
//!  * dithering in the blit pass removes gradient banding.

use eframe::egui::PaintCallbackInfo;
use eframe::egui_glow;
use eframe::glow::{self, HasContext};

const VS: &str = r#"
out vec2 v_uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FS: &str = r#"
uniform vec2 u_res;
uniform float u_time;
uniform vec2 u_mouse;
in vec2 v_uv;
out vec4 o;

float hash(vec2 p) {
    p = fract(p * vec2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return fract(p.x * p.y);
}
float noise(vec2 p) {
    vec2 i = floor(p), f = fract(p);
    vec2 u = f * f * (3.0 - 2.0 * f);
    return mix(mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x),
               mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x), u.y);
}
float fbm(vec2 p) {
    float v = 0.0, a = 0.5;
    mat2 m = mat2(1.6, 1.2, -1.2, 1.6);
    for (int i = 0; i < 5; i++) { v += a * noise(p); p = m * p; a *= 0.5; }
    return v;
}
void main() {
    vec2 p = (gl_FragCoord.xy - 0.5 * u_res) / u_res.y;
    float t = u_time * 0.045;

    // Domain warping: flowing "liquid" nebula.
    vec2 q = vec2(fbm(p * 1.4 + t), fbm(p * 1.4 + vec2(5.2, 1.3) - t));
    vec2 r = vec2(fbm(p * 1.4 + 3.0 * q + vec2(1.7, 9.2) + t * 1.6),
                  fbm(p * 1.4 + 3.0 * q + vec2(8.3, 2.8) - t * 1.3));
    float f = fbm(p * 1.4 + 3.2 * r);

    // "Indigo" palette (as in the logo): ink night, periwinkle, violet, lavender.
    vec3 night   = vec3(0.022, 0.020, 0.058);
    vec3 blurple = vec3(0.330, 0.350, 0.760);   // periwinkle
    vec3 pink    = vec3(0.420, 0.230, 0.680);   // violet
    vec3 cyan    = vec3(0.640, 0.670, 0.980);   // lavender

    vec3 col = mix(night, blurple, clamp(f * f * 2.3, 0.0, 1.0));
    col = mix(col, pink, clamp(length(q) * 0.6 - 0.28, 0.0, 1.0) * 0.55);
    col = mix(col, cyan, clamp(r.y * r.y * 0.95 - 0.28, 0.0, 1.0) * 0.45);
    col *= 0.45 + 1.15 * f * f;

    // Aurora ribbon.
    float band = sin(p.x * 2.3 + f * 5.0 + u_time * 0.12) * 0.16 + 0.18;
    col += mix(cyan, pink, v_uv.x) * 0.20 * exp(-abs(p.y - band) * 7.0) * (0.4 + f);

    // Glow following the mouse.
    vec2 mp = (u_mouse - 0.5) * vec2(u_res.x / u_res.y, 1.0);
    float md = length(p - mp);
    col += mix(blurple, pink, 0.5 + 0.5 * sin(u_time * 0.4)) * 0.22 * exp(-md * 3.2);
    col += vec3(0.85, 0.88, 1.0) * 0.05 * exp(-md * 14.0);

    // Twinkling stars (cheap: one hash per pixel).
    vec2 sp = p * 38.0 + vec2(u_time * 0.02, 0.0);
    vec2 sid = floor(sp), sf = fract(sp) - 0.5;
    float sh = hash(sid);
    if (sh > 0.92) {
        vec2 off = vec2(hash(sid + 7.1), hash(sid + 3.3)) - 0.5;
        float tw = 0.5 + 0.5 * sin(u_time * (0.8 + sh * 2.5) + sh * 60.0);
        col += vec3(0.85, 0.88, 1.0) * smoothstep(0.16, 0.0, length(sf - off * 0.6)) * tw * (0.35 + f);
    }

    // Vignette.
    vec2 d = v_uv - 0.5;
    col *= 1.0 - 0.8 * dot(d, d);
    col = pow(col, vec3(0.92));
    o = vec4(col, 1.0);
}
"#;

const BLIT_FS: &str = r#"
uniform sampler2D u_tex;
in vec2 v_uv;
out vec4 o;
float hash(vec2 p) {
    p = fract(p * vec2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return fract(p.x * p.y);
}
void main() {
    vec3 c = texture(u_tex, v_uv).rgb;
    c += (hash(gl_FragCoord.xy) - 0.5) / 255.0;
    o = vec4(c, 1.0);
}
"#;

pub struct Background {
    prog: glow::Program,
    blit: glow::Program,
    vao: glow::VertexArray,
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    size: (i32, i32),
    last: (f32, f32, f32, i32, i32),
    u_res: Option<glow::UniformLocation>,
    u_time: Option<glow::UniformLocation>,
    u_mouse: Option<glow::UniformLocation>,
    u_tex: Option<glow::UniformLocation>,
}

unsafe fn compile(gl: &glow::Context, header: &str, vs: &str, fs: &str) -> Result<glow::Program, String> {
    let prog = gl.create_program()?;
    let mut shaders = Vec::new();
    for (kind, src) in [(glow::VERTEX_SHADER, vs), (glow::FRAGMENT_SHADER, fs)] {
        let sh = gl.create_shader(kind)?;
        gl.shader_source(sh, &format!("{header}\nprecision highp float;\n{src}"));
        gl.compile_shader(sh);
        if !gl.get_shader_compile_status(sh) {
            let log = gl.get_shader_info_log(sh);
            gl.delete_shader(sh);
            gl.delete_program(prog);
            return Err(log);
        }
        gl.attach_shader(prog, sh);
        shaders.push(sh);
    }
    gl.link_program(prog);
    for sh in shaders {
        gl.detach_shader(prog, sh);
        gl.delete_shader(sh);
    }
    if !gl.get_program_link_status(prog) {
        let log = gl.get_program_info_log(prog);
        gl.delete_program(prog);
        return Err(log);
    }
    Ok(prog)
}

impl Background {
    pub fn new(gl: &glow::Context) -> Option<Self> {
        let egui_ver = egui_glow::ShaderVersion::get(gl).version_declaration();
        let headers = ["#version 330 core", "#version 300 es", egui_ver];
        unsafe {
            let (prog, blit) = headers.iter().find_map(|h| {
                let a = compile(gl, h, VS, FS).ok()?;
                match compile(gl, h, VS, BLIT_FS) {
                    Ok(b) => Some((a, b)),
                    Err(_) => {
                        gl.delete_program(a);
                        None
                    }
                }
            })?;
            Some(Self {
                u_res: gl.get_uniform_location(prog, "u_res"),
                u_time: gl.get_uniform_location(prog, "u_time"),
                u_mouse: gl.get_uniform_location(prog, "u_mouse"),
                u_tex: gl.get_uniform_location(blit, "u_tex"),
                prog,
                blit,
                vao: gl.create_vertex_array().ok()?,
                fbo: gl.create_framebuffer().ok()?,
                tex: gl.create_texture().ok()?,
                size: (0, 0),
                last: (f32::NAN, 0.0, 0.0, 0, 0),
            })
        }
    }

    pub fn paint(&mut self, gl: &glow::Context, info: &PaintCallbackInfo, time: f32, scale: f32, mouse: [f32; 2]) {
        let vp = info.viewport_in_pixels();
        let (w, h) = (vp.width_px.max(1), vp.height_px.max(1));
        let lw = ((w as f32 * scale).round() as i32).max(1);
        let lh = ((h as f32 * scale).round() as i32).max(1);
        unsafe {
            let prev_fb = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
            let prev_fb = std::num::NonZeroU32::new(prev_fb as u32).map(glow::NativeFramebuffer);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));

            if self.size != (lw, lh) {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    lw,
                    lh,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                for (k, v) in [
                    (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                    (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                    (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                    (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                ] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, k, v as i32);
                }
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.tex), 0);
                self.size = (lw, lh);
                self.last.0 = f32::NAN;
            }

            // Expensive pass — only when something changed.
            let key = (time, mouse[0], mouse[1], lw, lh);
            if self.last != key {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                gl.viewport(0, 0, lw, lh);
                gl.use_program(Some(self.prog));
                gl.uniform_2_f32(self.u_res.as_ref(), lw as f32, lh as f32);
                gl.uniform_1_f32(self.u_time.as_ref(), time);
                gl.uniform_2_f32(self.u_mouse.as_ref(), mouse[0], mouse[1]);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                self.last = key;
            }

            // Cheap pass: upscale to the screen.
            gl.bind_framebuffer(glow::FRAMEBUFFER, prev_fb);
            gl.viewport(vp.left_px, vp.from_bottom_px, w, h);
            gl.use_program(Some(self.blit));
            gl.uniform_1_i32(self.u_tex.as_ref(), 0);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);

            gl.enable(glow::SCISSOR_TEST);
            gl.enable(glow::BLEND);
        }
    }

    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_program(self.prog);
            gl.delete_program(self.blit);
            gl.delete_vertex_array(self.vao);
            gl.delete_framebuffer(self.fbo);
            gl.delete_texture(self.tex);
        }
    }
}
