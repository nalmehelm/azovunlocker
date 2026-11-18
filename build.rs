// Build step: prepares pictures and the icon, and embeds them into the exe.
//  * OUT_DIR/icon64.rgba     — window and taskbar icon (64×64, from assets/azovchan.png);
//  * OUT_DIR/logo.ico        — .exe icon (16–256 px, from assets/azovchan.png);
//  * OUT_DIR/azovchan.rgba,
//    OUT_DIR/miguelchan.rgba — top-bar pictures (160×160, white background removed).
// To change a picture or the icon, just replace the file in assets/ and rebuild.

use std::{env, fs, path::PathBuf};

/// .ico with 32-bit BMP entries (the format rc.exe and every version of Windows understand).
fn ico(src: &Plush) -> Vec<u8> {
    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let images: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let px = src.render(s as usize);
            let s = s as usize;
            let mask_row = s.div_ceil(32) * 4;
            let mut b = Vec::with_capacity(40 + s * s * 4 + mask_row * s);
            for v in [40u32, s as u32, (s * 2) as u32] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(&1u16.to_le_bytes());
            b.extend_from_slice(&32u16.to_le_bytes());
            b.extend_from_slice(&[0u8; 24]);
            for y in (0..s).rev() {
                for x in 0..s {
                    let o = (y * s + x) * 4;
                    b.extend_from_slice(&[px[o + 2], px[o + 1], px[o], px[o + 3]]);
                }
            }
            b.resize(b.len() + mask_row * s, 0);
            b
        })
        .collect();
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len();
    for (s, img) in sizes.iter().zip(&images) {
        let d = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[d, d, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += img.len();
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

/// A picture decoded once: white background removed, square crop around the doll.
/// [`Plush::render`] then makes any size from it.
struct Plush {
    w: usize,
    h: usize,
    px: Vec<[u8; 4]>,
    sx: i64,
    sy: i64,
    side: i64,
}

/// Top-bar picture: PNG → white background removed → cropped to the doll.
/// Everything is done at build time, so the program just takes a ready texture.
fn load(path: &str) -> Plush {
    let mut dec =
        png::Decoder::new(fs::File::open(path).unwrap_or_else(|_| panic!("{path} not found")));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().expect("bad PNG");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("bad PNG");
    let (w, h) = (info.width as usize, info.height as usize);
    let src = &buf[..info.buffer_size()];
    let mut px: Vec<[u8; 4]> = match info.color_type {
        png::ColorType::Rgba => src.as_chunks::<4>().0.to_vec(),
        png::ColorType::Rgb => src
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => src
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        _ => src.iter().map(|&g| [g, g, g, 255]).collect(),
    };

    // White background: if the image has no transparency of its own, fill inward from the edges
    // across near-white pixels and make them transparent. The fill only reaches the outside,
    // so white areas inside the doll (shirt, star) stay intact.
    if !px.iter().any(|p| p[3] < 250) {
        let white = |p: &[u8; 4]| p[0] >= 232 && p[1] >= 232 && p[2] >= 232;
        let mut bg = vec![false; w * h];
        let mut stack: Vec<usize> = (0..w)
            .flat_map(|x| [x, (h - 1) * w + x])
            .chain((0..h).flat_map(|y| [y * w, y * w + w - 1]))
            .collect();
        while let Some(i) = stack.pop() {
            if bg[i] || !white(&px[i]) {
                continue;
            }
            bg[i] = true;
            let (x, y) = (i % w, i / w);
            if x > 0 {
                stack.push(i - 1);
            }
            if x + 1 < w {
                stack.push(i + 1);
            }
            if y > 0 {
                stack.push(i - w);
            }
            if y + 1 < h {
                stack.push(i + w);
            }
        }
        for i in 0..w * h {
            if bg[i] {
                px[i][3] = 0;
                continue;
            }
            // Soft edge: light pixels touching the background become semi-transparent.
            let (x, y) = (i % w, i / w);
            let edge = (x > 0 && bg[i - 1])
                || (x + 1 < w && bg[i + 1])
                || (y > 0 && bg[i - w])
                || (y + 1 < h && bg[i + w]);
            let m = px[i][0].min(px[i][1]).min(px[i][2]);
            if edge && m > 190 {
                px[i][3] = ((255 - m) as u32 * 255 / 65).min(255) as u8;
            }
        }
    }

    // Crop to the doll + 3% margin, make it square.
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if px[y * w + x][3] > 16 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x1 <= x0 {
        (x0, y0, x1, y1) = (0, 0, w, h);
    }
    let side = ((x1 - x0).max(y1 - y0) as f32 * 1.06) as i64;
    let (cx, cy) = (((x0 + x1) / 2) as i64, ((y0 + y1) / 2) as i64);
    let (sx, sy) = (cx - side / 2, cy - side / 2);
    Plush {
        w,
        h,
        px,
        sx,
        sy,
        side,
    }
}

impl Plush {
    /// Square `out`×`out` RGBA (non-premultiplied).
    fn render(&self, out: usize) -> Vec<u8> {
        let Plush {
            w,
            h,
            ref px,
            sx,
            sy,
            side,
        } = *self;
        // Downscale by area averaging (in premultiplied colors so the edges don't go dark).
        let mut outv = vec![0u8; out * out * 4];
        let step = side as f32 / out as f32;
        for oy in 0..out {
            for ox in 0..out {
                let (fx0, fy0) = (sx as f32 + ox as f32 * step, sy as f32 + oy as f32 * step);
                let mut acc = [0f32; 4];
                let mut n = 0f32;
                let (ix0, iy0) = (fx0.floor() as i64, fy0.floor() as i64);
                let (ix1, iy1) = ((fx0 + step).ceil() as i64, (fy0 + step).ceil() as i64);
                for y in iy0..iy1 {
                    for x in ix0..ix1 {
                        n += 1.0;
                        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                            continue;
                        }
                        let p = px[y as usize * w + x as usize];
                        let a = p[3] as f32 / 255.0;
                        acc[0] += p[0] as f32 * a;
                        acc[1] += p[1] as f32 * a;
                        acc[2] += p[2] as f32 * a;
                        acc[3] += a;
                    }
                }
                let o = (oy * out + ox) * 4;
                if acc[3] > 0.0 {
                    for c in 0..3 {
                        outv[o + c] = (acc[c] / acc[3]).round().min(255.0) as u8;
                    }
                    outv[o + 3] = (acc[3] / n * 255.0).round().min(255.0) as u8;
                }
            }
        }
        outv
    }
}

fn main() {
    println!("cargo:rerun-if-changed=assets/azovchan.png");
    println!("cargo:rerun-if-changed=assets/miguelchan.png");
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    // App icon — azovchan (white background removed, cropped to the doll).
    let azov = load("assets/azovchan.png");
    fs::write(out.join("icon64.rgba"), azov.render(64)).unwrap();
    fs::write(out.join("azovchan.rgba"), azov.render(160)).unwrap();
    fs::write(
        out.join("miguelchan.rgba"),
        load("assets/miguelchan.png").render(160),
    )
    .unwrap();
    let ico_path = out.join("logo.ico");
    fs::write(&ico_path, ico(&azov)).unwrap();

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().unwrap());
        res.set("FileDescription", "azovunlocker");
        res.set("ProductName", "azovunlocker");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed icon: {e}");
        }
    }
}
