//! Patterns: Edit › Define Pattern, the pattern library (`pattern.*`), Layer › New Fill Layer ›
//! Pattern, Edit › Fill with a pattern and brush textures from library patterns.
//!
//! Two places hold patterns, as in Photoshop:
//! - the **library** ([`PatternLibrary`], on the [`Session`]): built-in procedural patterns plus
//!   defined and imported ones. Library edits are app state, not document history.
//! - the **document** ([`photocraft_doc::Document::patterns`]): the patterns its layers use
//!   (saved in PSD `Patt` blocks and `.pcraft`). Commands that apply a pattern copy it into the
//!   document in the same history step, so a document always renders on its own.
//!
//! Patterns are referenced by `"pattern": id | name` (document first, then library).

use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Color, Document, Fill, Layer, LayerContent, Pattern, Rect};
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// The app-level pattern library (Window › Patterns).
#[derive(Clone, Debug)]
pub struct PatternLibrary {
    pub items: Vec<Pattern>,
}

impl Default for PatternLibrary {
    fn default() -> Self {
        PatternLibrary { items: builtin() }
    }
}

fn hash01(x: u32, y: u32, seed: u32) -> f32 {
    let mut h = x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h & 0xffff) as f32 / 65535.0
}

/// Shortest signed distance between two tile coordinates of side `p`: the tile wraps at its edges,
/// so a pattern built from it is seamless.
fn wrapd(d: f32, p: f32) -> f32 {
    (d + p * 0.5).rem_euclid(p) - p * 0.5
}

/// Smoothed value noise that wraps at the tile side `p` (`cell` must divide `p`, `cell >= 1`).
fn vnoise(x: u32, y: u32, p: u32, cell: u32, seed: u32) -> f32 {
    let n = p / cell;
    let (cx, cy) = (x / cell, y / cell);
    let (fx, fy) = ((x % cell) as f32 / cell as f32, (y % cell) as f32 / cell as f32);
    let v = |i: u32, j: u32| hash01(i % n, j % n, seed);
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (smooth(fx), smooth(fy));
    let a = v(cx, cy) + (v(cx + 1, cy) - v(cx, cy)) * sx;
    let b = v(cx, cy + 1) + (v(cx + 1, cy + 1) - v(cx, cy + 1)) * sx;
    a + (b - a) * sy
}

/// Three octaves of [`vnoise`], for weathered surfaces (concrete, wood, stucco).
fn fbm(x: u32, y: u32, p: u32, seed: u32) -> f32 {
    0.55 * vnoise(x, y, p, 4, seed) + 0.3 * vnoise(x, y, p, 2, seed + 11) + 0.15 * vnoise(x, y, p, 1, seed + 23)
}

/// Photographic tiles from CC0 material scans (see `assets/patterns/` and the repository's
/// `ATTRIBUTION.md`). Decoded once and shared: a session clones the patterns, and a pattern's
/// tiles are `Arc`, so the clone stays cheap.
///
/// The web build leaves the four PNGs out: they are about 1.5 MB of the wasm and the browser
/// bundle has a hard per-file cap. The procedural tiles are identical on both.
#[cfg(not(target_arch = "wasm32"))]
fn photographic() -> &'static [Pattern] {
    static PHOTO: std::sync::OnceLock<Vec<Pattern>> = std::sync::OnceLock::new();
    PHOTO.get_or_init(|| {
        const FILES: [(&str, &[u8]); 4] = [
            ("Leaves", include_bytes!("../assets/patterns/leaves.png")),
            ("Marble Stone", include_bytes!("../assets/patterns/marble.png")),
            ("Wood Planks", include_bytes!("../assets/patterns/wood.png")),
            ("Concrete Rough", include_bytes!("../assets/patterns/concrete.png")),
        ];
        FILES
            .iter()
            .filter_map(|(name, bytes)| {
                let img = photocraft_codecs::decode(bytes).ok()?.convert(photocraft_codecs::ChannelLayout::Rgb, photocraft_codecs::SampleType::U8);
                let (w, h) = (img.width(), img.height());
                let mut surface = Surface::new(PixelFormat::new(ColorMode::Rgb, SampleType::U8, false));
                surface.write_interleaved(Rect::new(0, 0, w as i32, h as i32), img.data());
                Some(Pattern::new(*name, surface, w, h))
            })
            .collect()
    })
}

/// The web build has no room for the photographic tiles (see above).
#[cfg(target_arch = "wasm32")]
fn photographic() -> &'static [Pattern] {
    &[]
}

fn procedural(name: &str, w: u32, h: u32, f: impl Fn(u32, u32) -> [f32; 3]) -> Pattern {
    let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::U8, false);
    let mut s = Surface::new(fmt);
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&f(x, y));
        }
    }
    // Quantise through the 8-bit surface so ids are stable.
    s.write_region(Rect::new(0, 0, w as i32, h as i32), &px);
    Pattern::new(name, s, w, h)
}

/// Anti-aliased coverage from a signed distance in pixels: solid at `d >= 0.5`, empty at
/// `d <= -0.5`, so diagonals and curves stay smooth when the tile is used at 100 %.
fn ink(d: f32) -> f32 {
    (d + 0.5).clamp(0.0, 1.0)
}

/// A centred band of half-width `half` repeated every `period` along `t`: 1 inside, 0 outside,
/// with a one-pixel anti-aliased edge.
fn band(t: f32, period: f32, half: f32) -> f32 {
    let m = t.rem_euclid(period);
    ink(half - (m - period * 0.5).abs())
}

/// Builds the built-in patterns: our own formulas (clean-room), generated procedurally. The tiles
/// are large (384 px for the geometric ones) with anti-aliased edges, so the motifs read cleanly at
/// 100 % like Photoshop's presets; they are seamless and deterministic.
fn build_builtin() -> Vec<Pattern> {
    let g = |v: f32| [v, v, v];
    let mut patterns = vec![
        // Geometric. 384 px tiles, large motifs, one-pixel anti-aliased edges.
        procedural("Checkerboard", 384, 384, |x, y| g(if (x / 64 + y / 64) % 2 == 0 { 0.84 } else { 1.0 })),
        procedural("Stripes", 384, 384, |_, y| g(0.96 - 0.76 * band(y as f32 + 32.0, 64.0, 20.0))),
        procedural("Diagonal Lines", 384, 384, |x, y| g(0.96 - 0.76 * band((x + y) as f32, 64.0, 8.0))),
        procedural("Grid", 384, 384, |x, y| {
            let fx = (x as f32).rem_euclid(64.0);
            let fy = (y as f32).rem_euclid(64.0);
            let line = ink(4.0 - fx.min(64.0 - fx)).max(ink(4.0 - fy.min(64.0 - fy)));
            g(0.96 - 0.72 * line)
        }),
        procedural("Dots", 384, 384, |x, y| {
            let d = 24.0 - wrapd(x as f32 - 32.0, 64.0).hypot(wrapd(y as f32 - 32.0, 64.0));
            g(0.96 - 0.84 * ink(d))
        }),
        procedural("Chevron", 384, 384, |x, y| {
            let u = (x % 128) as f32 / 128.0;
            let tri = 1.0 - (2.0 * u - 1.0).abs();
            g(0.96 - 0.74 * band(y as f32 + 56.0 * tri + 8.0, 64.0, 16.0))
        }),
        procedural("Herringbone", 384, 384, |x, y| {
            let v = if ((x + y) / 32) % 2 == ((y + 384 - x) / 32) % 2 { 0.84 } else { 0.3 };
            [v, v * 0.97, v * 0.92]
        }),
        procedural("Wavy Lines", 384, 384, |x, y| {
            let t = y as f32 + 22.0 * (std::f32::consts::TAU * x as f32 / 128.0).sin();
            g(0.92 - 0.72 * band(t, 64.0, 20.0))
        }),
        procedural("Rings", 384, 384, |x, y| {
            let d = wrapd(x as f32 - 32.0, 64.0).hypot(wrapd(y as f32 - 32.0, 64.0));
            g(0.86 - 0.6 * ink(4.0 - (d - 24.0).abs()))
        }),
        procedural("Crosshatch", 384, 384, |x, y| {
            let a = band((x + y) as f32, 48.0, 4.0);
            let b = band(x as f32 - y as f32, 48.0, 4.0);
            g(0.96 - 0.72 * a.max(b))
        }),
        procedural("Truchet Arcs", 384, 384, |x, y| {
            let cell = 64u32;
            let n = 384 / cell;
            let (u, v) = ((x % cell) as f32 + 0.5, (y % cell) as f32 + 0.5);
            let r = cell as f32 * 0.5;
            let (a0, a1) = if hash01((x / cell) % n, (y / cell) % n, 1) < 0.5 {
                ([0.0, 0.0], [cell as f32, cell as f32])
            } else {
                ([cell as f32, 0.0], [0.0, cell as f32])
            };
            let d = ((u - a0[0]).hypot(v - a0[1]) - r).abs().min(((u - a1[0]).hypot(v - a1[1]) - r).abs());
            g(ink(3.5 - d))
        }),
        procedural("Truchet Lines", 384, 384, |x, y| {
            let cell = 64u32;
            let n = 384 / cell;
            let (u, v) = ((x % cell) as f32, (y % cell) as f32);
            let t = u / (cell as f32 - 1.0);
            let side = if hash01((x / cell) % n, (y / cell) % n, 5) < 0.5 { v - t * (cell as f32 - 1.0) } else { v - (1.0 - t) * (cell as f32 - 1.0) };
            g(ink(3.5 - side.abs()))
        }),
        procedural("Truchet Triangles", 384, 384, |x, y| {
            let cell = 64u32;
            let n = 384 / cell;
            let (u, v) = ((x % cell) as f32 + 0.5, (y % cell) as f32 + 0.5);
            let c = cell as f32;
            let d = if hash01((x / cell) % n, (y / cell) % n, 13) < 0.5 { u - v } else { u + v - c };
            g(0.9 - 0.66 * ((d + 1.0) * 0.5).clamp(0.0, 1.0))
        }),
        procedural("Interference", 384, 384, |x, y| {
            let tau = std::f32::consts::TAU;
            let s = |m: u32, o: u32| (tau * (m as f32 * x as f32 + o as f32 * y as f32) / 384.0).sin();
            let (a, b, c) = (s(12, 0), s(12, 2), s(2, 12));
            g((0.5 + 0.5 * (a * b + b * c + c * a) / 1.5).clamp(0.0, 1.0))
        }),
        procedural("Quasicrystal", 384, 384, |x, y| {
            const WAVES: [(i32, i32); 12] = [(1, 0), (0, 1), (1, 1), (1, -1), (2, 1), (1, 2), (2, -1), (-1, 2), (3, 1), (1, 3), (3, 2), (2, 3)];
            let mut s = 0.0f32;
            for &(m, n) in &WAVES {
                let phase = hash01(m.unsigned_abs() + 9, n.unsigned_abs() + 5, 9) * std::f32::consts::TAU;
                s += (std::f32::consts::TAU * 12.0 * (m as f32 * x as f32 + n as f32 * y as f32) / 384.0 + phase).cos();
            }
            g((0.5 + 0.5 * (2.2 * s / WAVES.len() as f32).tanh()).clamp(0.0, 1.0))
        }),
        procedural("Hexagons", 384, 384, |x, y| {
            let (w, h) = (64.0f32, 64.0f32);
            let mut d = f32::MIN;
            for gy in -1..=1i32 {
                for gx in -1..=1i32 {
                    let col = (x as f32 / 96.0).floor() as i32 + gx;
                    let row = (y as f32 / 128.0).floor() as i32 + gy;
                    let cx = col as f32 * 96.0;
                    let cy = row as f32 * 128.0 + if col.rem_euclid(2) == 0 { 0.0 } else { 64.0 };
                    let (dx, dy) = (wrapd(x as f32 - cx, 384.0), wrapd(y as f32 - cy, 384.0));
                    let (px, py) = (dx.abs(), dy.abs());
                    d = d.max((h - py).min((2.0 * w - 2.0 * px - py) / 5.0f32.sqrt()));
                }
            }
            g(0.86 - 0.6 * ink(2.0 - d.abs()))
        }),
        procedural("Islamic Star", 384, 384, |x, y| {
            let (l, s) = (128.0f32, 45.0f32);
            let k = std::f32::consts::FRAC_1_SQRT_2;
            let mut d = f32::MAX;
            for gy in -1..=1i32 {
                for gx in -1..=1i32 {
                    let cx = ((x as f32 / l).floor() as i32 + gx) as f32 * l + l * 0.5;
                    let cy = ((y as f32 / l).floor() as i32 + gy) as f32 * l + l * 0.5;
                    let (dx, dy) = (wrapd(x as f32 - cx, 384.0), wrapd(y as f32 - cy, 384.0));
                    let sq = dx.abs().max(dy.abs()) - s;
                    let sq45 = ((dx + dy) * k).abs().max(((dx - dy) * k).abs()) - s;
                    d = d.min(sq.min(sq45));
                }
            }
            let a = ink(-d);
            let gold = [0.85, 0.66, 0.22];
            let navy = [0.10, 0.13, 0.24];
            std::array::from_fn(|i| navy[i] + (gold[i] - navy[i]) * a)
        }),
        // Textures. 256 px tiles: fine grain, but big enough to read.
        procedural("Bricks", 256, 128, |x, y| {
            let row = y / 32;
            let xo = (x + if row % 2 == 1 { 64 } else { 0 }) % 128;
            let mortar = y % 32 < 3 || xo >= 125;
            let n = hash01(x, y, 7) * 0.08;
            if mortar { [0.78 + n, 0.76 + n, 0.72 + n] } else { [0.62 + n, 0.27 + n * 0.5, 0.2] }
        }),
        procedural("Weave", 64, 64, |x, y| {
            let over = ((x / 16) + (y / 16)) % 2 == 0;
            let t = if over { (y % 16) as f32 / 15.0 } else { (x % 16) as f32 / 15.0 };
            let v = 0.55 + 0.35 * (1.0 - (2.0 * t - 1.0).abs());
            [v, v * 0.92, v * 0.8]
        }),
        procedural("Basket Weave", 64, 64, |x, y| {
            let block = 32u32;
            let horizontal = ((x / block) + (y / block)).is_multiple_of(2);
            let (u, v) = (x % block, y % block);
            let (strand, phase) = if horizontal { (v / 16, u) } else { (u / 16, v) };
            let mut v0 = if strand == 0 { 0.72 } else { 0.5 };
            if phase == 15 || phase == 31 {
                v0 *= 0.55;
            }
            [v0 * 1.08, v0 * 0.92, v0 * 0.66]
        }),
        procedural("Paper Noise", 256, 256, |x, y| g(0.78 + 0.14 * vnoise(x, y, 256, 32, 1) + 0.08 * vnoise(x, y, 256, 8, 2))),
        procedural("Canvas", 32, 32, |x, y| {
            let over = ((x / 2) + (y / 2)) % 2 == 0;
            let v = if (if over { y % 2 } else { x % 2 }) == 0 { 0.85 } else { 0.55 } + hash01(x, y, 9) * 0.05;
            [v, v, v * 0.98]
        }),
        procedural("Denim", 64, 64, |x, y| {
            let v = 0.3 + 0.2 * (((x + 64 - y) % 32) as f32 / 31.0);
            let n = hash01(x, y, 13) * 0.12;
            [v * 0.85 + n, v * 0.95 + n, v * 1.25 + n]
        }),
        procedural("Concrete", 256, 256, |x, y| {
            let speckle = if hash01(x, y, 31) > 0.96 { 0.1 } else { 0.0 };
            g(0.6 + 0.22 * (fbm(x, y, 256, 5) - 0.5) + speckle)
        }),
        procedural("Stucco", 128, 128, |x, y| g(0.55 + 0.4 * (1.0 - (2.0 * fbm(x, y, 128, 7) - 1.0).abs()))),
        procedural("Sand", 256, 256, |x, y| {
            let v = 0.72 + 0.18 * (0.5 * hash01(x, y, 17) + 0.5 * hash01(x / 3, y / 3, 19) - 0.5);
            [v * 1.05, v * 0.98, v * 0.82]
        }),
        procedural("Wood Grain", 256, 256, |x, y| {
            let warp = fbm(x, y, 256, 3) - 0.5;
            let rings = 0.5 + 0.5 * (std::f32::consts::TAU * 16.0 * x as f32 / 256.0 + 6.0 * warp).sin();
            let grain = fbm(x, y, 256, 41);
            let (dark, light) = ([0.34, 0.19, 0.08], [0.72, 0.5, 0.28]);
            std::array::from_fn(|i| (dark[i] + (light[i] - dark[i]) * rings) * (0.85 + 0.25 * grain))
        }),
        procedural("Marble", 256, 256, |x, y| {
            let phase = std::f32::consts::TAU * 8.0 * (x as f32 + y as f32) / 256.0 + 5.0 * fbm(x, y, 256, 23);
            let vein = (1.0 - ((phase.sin().abs() - 0.08) / 0.14).clamp(0.0, 1.0)).max(0.0);
            g((0.85 + 0.09 * fbm(x, y, 256, 47)) * (1.0 - 0.72 * vein))
        }),
        // Organic.
        procedural("Seed Spiral", 256, 256, |x, y| {
            let size = 256.0;
            let centre = size * 0.5;
            let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
            let n = 200u32;
            let mut v = 0.0f32;
            for i in 1..=n {
                let f = i as f32 / n as f32;
                let r = size * 0.46 * f.sqrt();
                let a = i as f32 * golden;
                let d = (x as f32 - (centre + r * a.cos())).hypot(y as f32 - (centre + r * a.sin()));
                v = v.max((1.0 - ((d - 2.0) / 3.0).clamp(0.0, 1.0)) * (0.45 + 0.55 * f));
            }
            g(v.min(1.0))
        }),
        procedural("Scales", 256, 256, |x, y| {
            let (cw, ch, r) = (64.0f32, 64.0f32, 36.0f32);
            let (mut best, mut bdy) = (f32::MAX, 0.0f32);
            for gy in -1..=1i32 {
                for gx in -1..=1i32 {
                    let row = (y as f32 / ch).floor() as i32 + gy;
                    let shift = if row.rem_euclid(2) == 0 { 0.0 } else { cw * 0.5 };
                    let cx = ((x as f32 / cw).floor() as i32 + gx) as f32 * cw + cw * 0.5 + shift;
                    let cy = row as f32 * ch + ch * 0.5;
                    let (dx, dy) = (wrapd(x as f32 - cx, 256.0), wrapd(y as f32 - cy, 256.0));
                    let d = dx.hypot(dy);
                    if d < best {
                        best = d;
                        bdy = dy;
                    }
                }
            }
            let inside = (1.0 - ((best - r) / 2.0).clamp(0.0, 1.0)).max(0.0);
            let shade = 1.0 - 0.45 * ((bdy + r * 0.5) / (r * 1.5)).clamp(0.0, 1.0);
            let v = (0.12 + 0.72 * inside * shade).min(1.0);
            [v * 0.88, v * 0.94, v * 1.04]
        }),
        procedural("Cells", 256, 256, |x, y| {
            let cell = 64u32;
            let n = 256 / cell;
            let (mut f1, mut f2, mut id) = (f32::MAX, f32::MAX, 0u32);
            for gy in -1..=1i32 {
                for gx in -1..=1i32 {
                    let cx = (x / cell) as i32 + gx;
                    let cy = (y / cell) as i32 + gy;
                    let (px, py) = (cx.rem_euclid(n as i32) as u32, cy.rem_euclid(n as i32) as u32);
                    let fx = cx as f32 * cell as f32 + (0.2 + 0.6 * hash01(px, py, 61)) * cell as f32;
                    let fy = cy as f32 * cell as f32 + (0.2 + 0.6 * hash01(px, py, 67)) * cell as f32;
                    let d = wrapd(x as f32 - fx, 256.0).hypot(wrapd(y as f32 - fy, 256.0));
                    if d < f1 {
                        f2 = f1;
                        f1 = d;
                        id = py * n + px;
                    } else if d < f2 {
                        f2 = d;
                    }
                }
            }
            let wall = (1.0 - ((f2 - f1) / 3.0).clamp(0.0, 1.0)).powf(1.5);
            let shade = 0.55 + 0.35 * hash01(id, 7, 71);
            g((shade * (1.0 - 0.6 * wall)).min(1.0))
        }),
        // Halftone.
        procedural("Halftone Dots", 256, 256, |x, y| {
            let (u, v) = ((x % 32) as f32 - 15.5, (y % 32) as f32 - 15.5);
            g(0.96 - 0.84 * ((3.0 + 10.0 * (x as f32 / 256.0) - u.hypot(v)) / 1.5).clamp(0.0, 1.0))
        }),
        procedural("Halftone Lines", 256, 256, |x, y| {
            let d = ((y % 32) as f32 - 15.5).abs();
            g(0.96 - 0.84 * ((3.0 + 12.0 * (x as f32 / 256.0) - d) / 1.5).clamp(0.0, 1.0))
        }),
        // Effects.
        procedural("Bokeh", 256, 256, |x, y| {
            let size = 256.0;
            let mut v = 0.04f32;
            for k in 0..26u32 {
                let (cx, cy) = (hash01(k, 1, 21) * size, hash01(k, 2, 21) * size);
                let r = 8.0 + 24.0 * hash01(k, 3, 21);
                let d = wrapd(x as f32 - cx, size).hypot(wrapd(y as f32 - cy, size));
                v = v.max((0.35 + 0.5 * hash01(k, 4, 26)) * (1.0 - ((d - (r - 3.0)) / 3.0).clamp(0.0, 1.0)));
            }
            g(v.min(1.0))
        }),
        procedural("Grunge", 256, 256, |x, y| g(((1.0 - ((fbm(x, y, 256, 31) - 0.45) / 0.2).clamp(0.0, 1.0)) * (0.7 + 0.5 * hash01(x, y, 34))).min(1.0))),
    ];
    patterns.extend(photographic().iter().cloned());
    patterns
}

/// The built-in pattern library, generated once and shared: a session clones the patterns, and a
/// pattern's tiles are `Arc`, so the clone stays cheap even for the 384 px tiles.
pub fn builtin() -> Vec<Pattern> {
    static CACHE: std::sync::OnceLock<Vec<Pattern>> = std::sync::OnceLock::new();
    CACHE.get_or_init(build_builtin).clone()
}

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn always(_: &Session) -> std::result::Result<(), String> {
    Ok(())
}

fn has_doc(s: &Session) -> std::result::Result<(), String> {
    s.active().map(|_| ()).ok_or_else(|| "no document open".into())
}

fn has_pattern(s: &Session) -> std::result::Result<(), String> {
    if s.patterns.items.is_empty() && s.active().is_none_or(|d| d.doc.patterns.is_empty()) { Err("no patterns".into()) } else { Ok(()) }
}

/// Finds a pattern by id or name: the active document's first, then the library. An empty key
/// picks the first library pattern (Photoshop's default).
pub fn resolve(s: &Session, key: &str) -> Option<Pattern> {
    if key.is_empty() {
        // The Patterns panel's selection, else the first library pattern.
        return crate::presets::patterns::current(s).or(s.patterns.items.first()).cloned();
    }
    if let Some(d) = s.active()
        && let Some(p) = photocraft_doc::pattern::find(&d.doc.patterns, key, key)
    {
        return Some(p.clone());
    }
    photocraft_doc::pattern::find(&s.patterns.items, key, key).cloned()
}

pub(crate) fn resolve_param(s: &Session, cmd: &str, p: &Value) -> Result<Pattern> {
    let key = p.get("pattern").and_then(Value::as_str).unwrap_or("");
    resolve(s, key).ok_or_else(|| bad(cmd, format!("no pattern \"{key}\" (see pattern.list)")))
}

/// Copies `pat` into the document's patterns unless it is already there (by id).
pub fn ensure_in_doc(doc: &mut Document, pat: &Pattern) {
    if !doc.patterns.iter().any(|p| p.id == pat.id) {
        doc.patterns.push(pat.clone());
    }
}

/// Resolves the pattern of a Pattern Overlay / pattern stroke built from params (its `name`
/// holds the requested key) into a real pattern: returns the effect with name and id filled in,
/// plus the pattern to copy into the document.
pub fn resolve_effect(s: &Session, fx: photocraft_doc::Effect) -> Result<(photocraft_doc::Effect, Option<Pattern>)> {
    use photocraft_doc::Effect;
    match fx {
        Effect::PatternOverlay { common, name, id, scale, angle, link, phase } => {
            let key = if id.is_empty() { name } else { id };
            let pat = resolve(s, &key).ok_or_else(|| bad("layer.layerStyle.patternOverlay", format!("no pattern \"{key}\" (see pattern.list)")))?;
            Ok((Effect::PatternOverlay { common, name: pat.name.clone(), id: pat.id.clone(), scale, angle, link, phase }, Some(pat)))
        }
        other => Ok((other, None)),
    }
}

fn info(p: &Pattern, in_doc: bool, in_library: bool) -> Value {
    let f = p.surface.format();
    json!({
        "id": p.id, "name": p.display_name(), "storedName": p.name, "width": p.width, "height": p.height,
        "mode": format!("{:?}", f.mode), "depth": format!("{:?}", f.sample), "alpha": f.alpha,
        "inDocument": in_doc, "inLibrary": in_library,
    })
}

fn list(s: &mut Session, _: &Value) -> Result<Value> {
    let doc_pats: Vec<Pattern> = s.active().map(|d| d.doc.patterns.clone()).unwrap_or_default();
    let mut out: Vec<Value> = s.patterns.items.iter().map(|p| info(p, doc_pats.iter().any(|d| d.id == p.id), true)).collect();
    out.extend(doc_pats.iter().filter(|d| !s.patterns.items.iter().any(|p| p.id == d.id)).map(|p| info(p, true, false)));
    Ok(Value::Array(out))
}

/// The visible composite over `r` as a surface in the document's pixel format (with alpha).
fn composite_surface(doc: &Document, r: Rect) -> Surface {
    let buf = photocraft_compose::render(doc, r);
    let df = doc.pixel_format();
    let fmt = PixelFormat::new(df.mode, df.sample, true);
    let n = fmt.channels();
    let mut px = vec![0.0f32; buf.px.len() * n];
    for (o, p) in px.chunks_exact_mut(n).zip(&buf.px) {
        photocraft_raster::from_rgba_into(&fmt, *p, o);
    }
    let mut s = Surface::new(fmt);
    s.write_region(Rect::new(0, 0, r.width() as i32, r.height() as i32), &px);
    s
}

/// The name Define Pattern gives a pattern without one: the active document's name without its
/// extension ("Pattern" without a document), numbered past the library's existing names. The
/// Define Pattern dialog starts from it.
pub fn default_name(s: &Session) -> String {
    let doc_name = s.active().map(|st| st.doc.name.clone()).unwrap_or_default();
    let base =
        std::path::Path::new(&doc_name).file_stem().map(|x| x.to_string_lossy().to_string()).filter(|b| !b.is_empty()).unwrap_or_else(|| "Pattern".into());
    let taken = |n: &str| s.patterns.items.iter().any(|q| q.name == n);
    if !taken(&base) {
        return base;
    }
    (2..).map(|i| format!("{base} {i}")).find(|n| !taken(n)).unwrap_or(base)
}

fn define(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "edit.definePattern";
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let doc = &st.doc;
    let r = match p.get("rect").and_then(Value::as_array) {
        Some(a) if a.len() == 4 => {
            let v: Vec<i32> = a.iter().map(|x| x.as_f64().unwrap_or(0.0).round() as i32).collect();
            Rect::new(v[0], v[1], v[2], v[3])
        }
        _ => doc.selection.as_ref().map_or(doc.bounds(), |m| m.content_bounds()),
    }
    .intersect(&doc.bounds());
    if r.is_empty() {
        return Err(bad(cmd, "the area is empty"));
    }
    if r.width() > 4000 || r.height() > 4000 {
        return Err(bad(cmd, "patterns are limited to 4000 × 4000 px"));
    }
    let mut surf = composite_surface(doc, r);
    // Opaque patterns drop their alpha channel (as Photoshop does).
    let fmt = surf.format();
    let px = surf.read_region(Rect::new(0, 0, r.width() as i32, r.height() as i32));
    if px.chunks_exact(fmt.channels()).all(|q| q[fmt.channels() - 1] >= 1.0) {
        surf = surf.convert(PixelFormat::new(fmt.mode, fmt.sample, false));
    }
    let name = match p.get("name").and_then(Value::as_str) {
        Some(n) if !n.trim().is_empty() => n.to_string(),
        _ => default_name(s),
    };
    let pat = Pattern::new(name, surf, r.width(), r.height());
    let id = pat.id.clone();
    let out = info(&pat, false, true);
    if !s.patterns.items.iter().any(|q| q.id == id) {
        s.patterns.items.push(pat);
    }
    Ok(json!({"pattern": id, "info": out}))
}

fn library_index(s: &Session, cmd: &str, p: &Value) -> Result<usize> {
    let key = p.get("pattern").and_then(Value::as_str).ok_or_else(|| bad(cmd, "missing `pattern` (id or name)"))?;
    let found =
        photocraft_doc::pattern::find(&s.patterns.items, key, key).map(|f| f.id.clone()).ok_or_else(|| bad(cmd, format!("no library pattern \"{key}\"")))?;
    Ok(s.patterns.items.iter().position(|q| q.id == found).unwrap_or(0))
}

fn rename(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "pattern.rename";
    let name = p.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).ok_or_else(|| bad(cmd, "missing `name`"))?.to_string();
    let i = library_index(s, cmd, p)?;
    let id = s.patterns.items[i].id.clone();
    s.patterns.items[i].name = name.clone();
    // A copy stored in the document is renamed too (a document edit, so undoable).
    if s.active().is_some_and(|d| d.doc.patterns.iter().any(|q| q.id == id)) {
        s.edit("Rename Pattern", |doc, _| {
            for q in doc.patterns.iter_mut().filter(|q| q.id == id) {
                q.name = name.clone();
            }
            Ok(())
        })?;
    }
    Ok(json!({"pattern": id, "name": name}))
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    let i = library_index(s, "pattern.delete", p)?;
    let pat = s.patterns.items.remove(i);
    Ok(json!({"deleted": pat.id}))
}

fn import(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "pattern.import";
    // A file on disk, or its bytes (a dropped file, the web build).
    let bytes = match (p.get("path").and_then(Value::as_str), p.get("data").and_then(Value::as_str)) {
        (Some(path), _) => std::fs::read(path).map_err(|e| EngineError::Other(format!("{path}: {e}")))?,
        (None, Some(b64)) => photocraft_paint::tile::b64_decode(b64).ok_or_else(|| bad(cmd, "`data` is not base64"))?,
        (None, None) => return Err(bad(cmd, "missing `path` (.pat file) or `data`")),
    };
    let pats = photocraft_io::pattern_map::read_pat(&bytes).map_err(EngineError::Other)?;
    let mut ids = Vec::new();
    for pat in pats {
        ids.push(pat.id.clone());
        match s.patterns.items.iter_mut().find(|q| q.id == pat.id) {
            Some(q) => *q = pat,
            None => s.patterns.items.push(pat),
        }
    }
    Ok(json!({"imported": ids, "count": ids.len()}))
}

fn export(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "pattern.export";
    let path = p.get("path").and_then(Value::as_str).ok_or_else(|| bad(cmd, "missing `path`"))?;
    let pats: Vec<Pattern> = match p.get("patterns").and_then(Value::as_array) {
        Some(keys) => {
            keys.iter().filter_map(Value::as_str).map(|k| resolve(s, k).ok_or_else(|| bad(cmd, format!("no pattern \"{k}\"")))).collect::<Result<_>>()?
        }
        None => s.patterns.items.clone(),
    };
    let bytes = photocraft_io::pattern_map::write_pat(&pats).map_err(EngineError::Other)?;
    crate::file_cmds::write_file(path, &bytes)?;
    Ok(json!({"path": path, "count": pats.len(), "bytes": bytes.len()}))
}

pub(crate) fn placement(p: &Value) -> (f32, f32, bool, (f32, f32)) {
    let num = |k: &str, d: f64| p.get(k).and_then(Value::as_f64).unwrap_or(d) as f32;
    let phase = p
        .get("phase")
        .and_then(Value::as_array)
        .map_or((0.0, 0.0), |a| (a.first().and_then(Value::as_f64).unwrap_or(0.0) as f32, a.get(1).and_then(Value::as_f64).unwrap_or(0.0) as f32));
    ((num("scale", 100.0) / 100.0).clamp(0.01, 10.0), num("angle", 0.0), p.get("link").and_then(Value::as_bool).unwrap_or(true), phase)
}

/// The edit of `layer.newFillLayer.pattern`: a Pattern Fill layer of `pat` placed as `p` asks,
/// above `active`. The Pattern Fill dialog previews it on a copy of the document.
pub(crate) fn insert_fill_layer(doc: &mut Document, active: &mut Option<photocraft_doc::LayerId>, pat: &Pattern, p: &Value) -> Result<photocraft_doc::LayerId> {
    let (scale, angle, link, phase) = placement(p);
    ensure_in_doc(doc, pat);
    let fill = Fill::Pattern { name: pat.name.clone(), scale, id: pat.id.clone(), angle, link, phase };
    let mut l = Layer::new(doc.next_layer_name("Pattern Fill"), LayerContent::Fill(fill));
    crate::commands::mask_new_layer(doc, &mut l, p, "layer.newFillLayer.pattern")?;
    let id = doc.insert_above(*active, l);
    *active = Some(id);
    Ok(id)
}

fn new_fill_layer(s: &mut Session, p: &Value) -> Result<Value> {
    let pat = resolve_param(s, "layer.newFillLayer.pattern", p)?;
    let id = s.edit("New Pattern Fill Layer", |doc, active| insert_fill_layer(doc, active, &pat, p))?;
    Ok(json!({"layer": id.0, "pattern": pat.id}))
}

fn brush_texture(s: &mut Session, p: &Value) -> Result<Value> {
    let pat = resolve_param(s, "brush.texturePattern", p)?;
    let n = pat.is_paintable().then(|| usize::try_from(pat.pixel_count()).ok()).flatten();
    let Some(n) = n else {
        return Err(bad("brush.texturePattern", "the pattern is empty or too large to paint"));
    };
    // Brush textures are grey: luminance of the composite over white.
    let mut px = vec![[0.0f32; 4]; n];
    pat.surface.read_rgba_into(pat.rect(), &mut px);
    let lum: Vec<f32> = px
        .iter()
        .map(|q| {
            let l = 0.299 * q[0] + 0.587 * q[1] + 0.114 * q[2];
            l * q[3] + (1.0 - q[3])
        })
        .collect();
    let tile = photocraft_paint::GrayTile::from_f32(pat.width, pat.height, &lum);
    let t = &mut s.tools.brush.texture;
    t.pattern = photocraft_paint::Pattern::Tile(tile);
    t.enabled = p.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    if let Some(sc) = p.get("scale").and_then(Value::as_f64) {
        t.scale = (sc as f32 / 100.0).clamp(0.01, 10.0);
    }
    Ok(json!({"pattern": pat.id}))
}

/// A swatch colour for UIs (average of the tile), straight RGB.
pub fn average_color(p: &Pattern) -> Color {
    // A pattern too large to paint is averaged over its paintable top-left corner.
    let side = photocraft_doc::pattern::MAX_PAINT_PIXELS.isqrt() as u32;
    let (w, h) = (p.width.min(side), p.height.min(side));
    let mut px = vec![[0.0f32; 4]; w as usize * h as usize];
    p.surface.read_rgba_into(Rect::new(0, 0, w as i32, h as i32), &mut px);
    let n = px.len().max(1) as f32;
    let s = px.iter().fold([0.0f32; 3], |a, q| [a[0] + q[0], a[1] + q[1], a[2] + q[2]]);
    Color::rgb(s[0] / n, s[1] / n, s[2] / n)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "edit.definePattern",
            label: "Define Pattern…",
            menu: &["Edit"],
            shortcut: None,
            params: r##"{"name":text,"rect":[x0,y0,x1,y1]?} → {"pattern":id} (name: default the document's name numbered past the library's names; rect: default the selection bounds or else the canvas; added to the library from the visible composite)"##,
            enabled: has_doc,
            journal: true,
            run: define,
        },
        CommandSpec {
            id: "pattern.list",
            label: "Patterns",
            menu: &[],
            shortcut: None,
            params: "{} → [{id,name,width,height,mode,depth,inDocument,inLibrary}]",
            enabled: always,
            journal: false,
            run: list,
        },
        CommandSpec {
            id: "pattern.rename",
            label: "Rename Pattern",
            menu: &[],
            shortcut: None,
            params: r##"{"pattern":id|name,"name":str}"##,
            enabled: has_pattern,
            journal: true,
            run: rename,
        },
        CommandSpec {
            id: "pattern.delete",
            label: "Delete Pattern",
            menu: &[],
            shortcut: None,
            params: r##"{"pattern":id|name} (library only; documents keep their copy)"##,
            enabled: has_pattern,
            journal: true,
            run: delete,
        },
        CommandSpec {
            id: "pattern.import",
            label: "Import Patterns…",
            menu: &[],
            shortcut: None,
            params: r##"{"path":".pat file"?,"data":base64 bytes? (instead of a path)} → {imported:[ids],count}"##,
            enabled: always,
            journal: true,
            run: import,
        },
        CommandSpec {
            id: "pattern.export",
            label: "Export Patterns…",
            menu: &[],
            shortcut: None,
            params: r##"{"path":".pat file","patterns":[id|name]? (default: the whole library)}"##,
            enabled: has_pattern,
            journal: true,
            run: export,
        },
        CommandSpec {
            id: "layer.newFillLayer.pattern",
            label: "Pattern…",
            menu: &["Layer", "New Fill Layer"],
            shortcut: None,
            params: r##"{"pattern":id|name?=first library pattern,"scale":1..1000=100,"angle":deg=0,"link":bool=true,"phase":[x,y]?,"path":"work"|saved path name|{…path}? (the active path: becomes the vector mask; else the selection is the layer mask)}"##,
            enabled: has_doc,
            journal: true,
            run: new_fill_layer,
        },
        CommandSpec {
            id: "brush.texturePattern",
            label: "Brush Texture Pattern",
            menu: &[],
            shortcut: None,
            params: r##"{"pattern":id|name,"scale":1..1000?,"enabled":bool=true} (sets the session brush's Texture to that pattern)"##,
            enabled: has_pattern,
            journal: true,
            run: brush_texture,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_doc::LayerId;

    fn session(depth: u64) -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 40, "height": 30, "depth": depth})).unwrap();
        s
    }

    fn pixel(s: &Session, x: i32, y: i32) -> [f32; 4] {
        let b = photocraft_compose::render(&s.active().unwrap().doc, Rect::new(x, y, x + 1, y + 1));
        b.px[0]
    }

    #[test]
    fn library_has_builtins_and_lists() {
        let mut s = Session::new();
        let v = s.execute("pattern.list", json!({})).unwrap();
        assert!(v.as_array().unwrap().len() >= 6);
        assert!(v[0]["id"].as_str().unwrap().len() == 36);
        // Built-in ids are stable across sessions.
        assert_eq!(builtin()[0].id, Session::new().patterns.items[0].id);
    }

    #[test]
    fn builtin_patterns_are_unique_deterministic_and_covered_by_groups() {
        let first = builtin();
        let second = builtin();
        assert!(first.len() >= 20, "the library keeps growing: {}", first.len());
        assert_eq!(
            first.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            second.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            "ids are stable (a built-in never changes)"
        );
        let names: std::collections::BTreeSet<&str> = first.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), first.len(), "pattern names are unique");
        let ids: std::collections::BTreeSet<String> = first.iter().map(|p| p.id.clone()).collect();
        let mut grouped: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for group in crate::presets::patterns::builtin_groups() {
            for id in group.items {
                assert!(grouped.insert(id.clone()), "`{id}` is in two groups");
            }
        }
        assert_eq!(grouped, ids, "every built-in pattern is in exactly one group");
    }

    #[test]
    fn photo_and_stripes_builtins_have_the_expected_pixels() {
        let s = Session::new();
        // Leaves: the embedded CC0 photo tile decodes to 512×512 with a real tonal range.
        let leaves = resolve(&s, "Leaves").unwrap();
        assert_eq!((leaves.width, leaves.height), (512, 512));
        let (mut lo, mut hi) = (1.0f32, 0.0f32);
        for y in (0..512).step_by(8) {
            for x in (0..512).step_by(8) {
                let px = leaves.surface.pixel(x, y);
                let l = (px[0] + px[1] + px[2]) / 3.0;
                lo = lo.min(l);
                hi = hi.max(l);
            }
        }
        assert!(lo < 0.2 && hi > 0.45, "photo tonal range {lo}..{hi}");
        // Stripes: 32 px dark bands on a light field.
        let stripes = resolve(&s, "Stripes").unwrap();
        assert!((stripes.surface.pixel(0, 0)[0] - 0.2).abs() < 0.03, "{:?}", stripes.surface.pixel(0, 0));
        assert!((stripes.surface.pixel(0, 40)[0] - 0.96).abs() < 0.03, "{:?}", stripes.surface.pixel(0, 40));
    }

    #[test]
    fn new_builtins_have_the_expected_pixels() {
        let s = Session::new();
        // Hexagons: a large flat-top honeycomb — light cells with dark shared edges.
        let hex = resolve(&s, "Hexagons").unwrap();
        assert_eq!((hex.width, hex.height), (384, 384));
        let (mut light, mut dark) = (0, 0);
        for y in (0..384).step_by(4) {
            for x in (0..384).step_by(4) {
                let v = hex.surface.pixel(x, y)[0];
                if v > 0.7 {
                    light += 1;
                } else if v < 0.4 {
                    dark += 1;
                }
            }
        }
        assert!(light > 1000 && dark > 100, "honeycomb cells {light} light / {dark} dark");
        // Islamic Star: gold stars on a navy field.
        let star = resolve(&s, "Islamic Star").unwrap();
        assert_eq!((star.width, star.height), (384, 384));
        let (mut gold, mut navy) = (0, 0);
        for y in (0..384).step_by(4) {
            for x in (0..384).step_by(4) {
                let p = star.surface.pixel(x, y);
                if p[0] > 0.8 && p[1] > 0.6 && p[2] < 0.3 {
                    gold += 1;
                } else if p[0] < 0.15 && p[2] > 0.2 {
                    navy += 1;
                }
            }
        }
        assert!(gold > 200 && navy > 200, "star {gold} gold / {navy} navy");
        // Marble: light stone threaded with thin dark veins.
        let marble = resolve(&s, "Marble").unwrap();
        assert_eq!((marble.width, marble.height), (256, 256));
        let (mut dark, mut light) = (0, 0);
        for y in 0..256 {
            for x in 0..256 {
                let px = marble.surface.pixel(x, y)[0];
                if px < 0.35 {
                    dark += 1;
                } else if px > 0.7 {
                    light += 1;
                }
            }
        }
        assert!(dark > 100, "some dark veins: {dark} dark pixels");
        assert!(light > 20000, "mostly light stone: {light} light pixels");
    }

    #[test]
    fn define_rename_delete_and_pat_round_trip() {
        let mut s = session(8);
        s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
        s.execute("select.rect", json!({"x": 2, "y": 3, "width": 5, "height": 4})).unwrap();
        let r = s.execute("edit.definePattern", json!({"name": "Red"})).unwrap();
        let id = r["pattern"].as_str().unwrap().to_string();
        let pat = resolve(&s, &id).unwrap();
        assert_eq!((pat.width, pat.height), (5, 4));
        assert!(!pat.surface.format().alpha, "opaque pattern drops alpha");
        // Same content → same id (replayable); not duplicated.
        let n = s.patterns.items.len();
        s.execute("edit.definePattern", json!({"name": "Red"})).unwrap();
        assert_eq!(s.patterns.items.len(), n);
        s.execute("pattern.rename", json!({"pattern": id, "name": "Crimson"})).unwrap();
        assert_eq!(resolve(&s, "Crimson").unwrap().id, id);
        let dir = std::env::temp_dir().join(format!("pc-pat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lib.pat");
        let path = path.to_str().unwrap();
        s.execute("pattern.export", json!({"path": path})).unwrap();
        s.execute("pattern.delete", json!({"pattern": "Crimson"})).unwrap();
        assert!(resolve(&s, "Crimson").is_none());
        assert!(s.execute("pattern.delete", json!({"pattern": "Crimson"})).is_err());
        let v = s.execute("pattern.import", json!({"path": path})).unwrap();
        assert!(v["imported"].as_array().unwrap().iter().any(|x| x == id.as_str()));
        assert_eq!(resolve(&s, &id).unwrap().surface.read_region(Rect::new(0, 0, 5, 4)), pat.surface.read_region(Rect::new(0, 0, 5, 4)));
        // The same file as bytes (a dropped file, the web build).
        let bytes = std::fs::read(path).unwrap();
        s.execute("pattern.delete", json!({"pattern": "Crimson"})).unwrap();
        let v = s.execute("pattern.import", json!({"data": photocraft_paint::tile::b64_encode(&bytes)})).unwrap();
        assert_eq!(v["count"], v["imported"].as_array().unwrap().len());
        assert_eq!(resolve(&s, "Crimson").unwrap().id, id);
        std::fs::remove_dir_all(dir).ok();
        assert!(s.execute("pattern.import", json!({"path": "/nonexistent.pat"})).is_err());
        for bad in [json!({}), json!({"data": "not base64!"}), json!({"data": "aGVsbG8="})] {
            assert!(s.execute("pattern.import", bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn pattern_fill_layer_renders_at_every_depth_and_undoes() {
        for depth in [8u64, 16, 32] {
            let mut s = session(depth);
            let r = s.execute("layer.newFillLayer.pattern", json!({"pattern": "Checkerboard"})).unwrap();
            let id = LayerId(r["layer"].as_u64().unwrap());
            let st = s.active().unwrap();
            assert_eq!(st.doc.patterns.len(), 1, "copied into the document");
            assert!(matches!(&st.doc.layer(id).unwrap().content, LayerContent::Fill(Fill::Pattern { .. })));
            // Checkerboard: 64 px squares of 0.84 and 1.0 grey from the canvas origin.
            let a = pixel(&s, 1, 1);
            assert!((a[0] - 0.84).abs() < 0.01, "{depth}: {a:?}");
            s.undo();
            assert!(s.active().unwrap().doc.patterns.is_empty());
            assert!(s.execute("layer.newFillLayer.pattern", json!({"pattern": "nope"})).is_err());
        }
    }

    #[test]
    fn fill_with_pattern_respects_selection() {
        for depth in [8u64, 16, 32] {
            let mut s = session(depth);
            s.execute("select.rect", json!({"x": 0, "y": 0, "width": 8, "height": 8})).unwrap();
            s.execute("edit.fill", json!({"contents": "pattern", "pattern": "Checkerboard"})).unwrap();
            let st = s.active().unwrap();
            let surf = st.doc.layers[0].surface().unwrap();
            let inside = surf.pixel(1, 1)[0];
            assert!(inside < 0.9, "{depth}: pattern fills the selection {inside}");
            assert!(surf.pixel(20, 20)[0] > 0.99, "outside the selection untouched");
        }
    }

    #[test]
    fn pattern_overlay_composes_with_an_outside_stroke() {
        let mut s = session(8);
        s.execute("layer.new.layer", json!({})).unwrap();
        s.execute("select.rect", json!({"x": 10, "y": 10, "width": 10, "height": 10})).unwrap();
        s.execute("edit.fill", json!({"color": "#0000ff"})).unwrap();
        s.execute("select.deselect", json!({})).unwrap();
        s.execute("layer.layerStyle.patternOverlay", json!({"pattern": "Diagonal Lines"})).unwrap();
        s.execute("layer.layerStyle.stroke", json!({"size": 3, "color": "#00ff00", "add": true})).unwrap();
        let out = pixel(&s, 8, 15);
        assert!(out[1] > 0.9 && out[0] < 0.1, "stroke outside: {out:?}");
    }

    #[test]
    fn pattern_overlay_renders_and_brush_texture_uses_library() {
        let mut s = session(16);
        s.execute("layer.new.layer", json!({})).unwrap();
        s.execute("edit.fill", json!({"color": "#0000ff"})).unwrap();
        s.execute("layer.layerStyle.patternOverlay", json!({"pattern": "Checkerboard", "opacity": 100})).unwrap();
        let st = s.active().unwrap();
        assert_eq!(st.doc.patterns.len(), 1);
        let p = pixel(&s, 1, 1);
        assert!((p[0] - 0.84).abs() < 0.01 && (p[2] - 0.84).abs() < 0.01, "{p:?}");
        assert!(s.execute("layer.layerStyle.patternOverlay", json!({"pattern": "missing"})).is_err());
        s.execute("layer.layerStyle.stroke", json!({"size": 4, "color": "#ffffff", "add": true})).unwrap();
        let st = s.active().unwrap();
        assert_eq!(st.doc.layer(st.active_layer.unwrap()).unwrap().effects.items.len(), 2);
        s.execute("brush.texturePattern", json!({"pattern": "Dots", "scale": 50})).unwrap();
        assert!(s.tools.brush.texture.enabled);
        assert!(matches!(s.tools.brush.texture.pattern, photocraft_paint::Pattern::Tile(_)));
    }

    #[test]
    fn a_pattern_too_large_to_paint_is_refused_not_allocated() {
        // width × height overflows u32: this panicked (debug) or wrapped (release).
        let mut s = session(8);
        let huge = Pattern { id: "huge".into(), name: "Huge".into(), width: 65_536, height: 65_536, surface: Surface::new(PixelFormat::RGBA8) };
        s.patterns.items.push(huge);
        let e = s.execute("brush.texturePattern", json!({"pattern": "huge"})).unwrap_err();
        assert!(e.to_string().contains("too large"), "{e}");
    }
}
