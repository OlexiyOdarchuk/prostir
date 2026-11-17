//! The "panorama" background: an equirectangular image shown by the compositor
//! as a sphere around the user. Generated here (a calm night sky) unless the
//! user supplies their own picture.

/// Width × height of the generated panorama (2:1 equirectangular).
pub const WIDTH: u32 = 4096;
pub const HEIGHT: u32 = 2048;

/// Small deterministic PRNG (xorshift) so the sky looks the same every launch.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Smooth value noise in [0, 1], wrapping horizontally every `period` cells.
fn noise(x: f32, y: f32, period: u32, seed: u64) -> f32 {
    let hash = |ix: i64, iy: i64| {
        let ix = ix.rem_euclid(period as i64) as u64;
        let mut h = ix.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (iy as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ seed;
        h ^= h >> 29;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 32;
        (h >> 40) as f32 / (1u64 << 24) as f32
    };
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (ix, iy) = (x0 as i64, y0 as i64);
    let a = hash(ix, iy) + (hash(ix + 1, iy) - hash(ix, iy)) * s(fx);
    let b = hash(ix, iy + 1) + (hash(ix + 1, iy + 1) - hash(ix, iy + 1)) * s(fx);
    a + (b - a) * s(fy)
}

/// RGBA8 (opaque, sRGB), top row = straight up.
pub fn night_sky() -> Vec<u8> {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let zenith = [0.014, 0.019, 0.042];
    let sky = [0.030, 0.037, 0.080];
    let horizon = [0.085, 0.075, 0.140];
    let glow = [0.300, 0.170, 0.230];
    let ground = [0.018, 0.020, 0.032];

    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        // Elevation: +1 straight up, 0 horizon, -1 straight down.
        let elev = 1.0 - 2.0 * (y as f32 + 0.5) / h as f32;
        for x in 0..w {
            let u = x as f32 / w as f32;
            let mut c = if elev >= 0.0 {
                let base = mix(horizon, sky, (elev / 0.25).powf(0.7));
                mix(base, zenith, ((elev - 0.25) / 0.75).max(0.0))
            } else {
                mix(horizon, ground, (-elev / 0.08).powf(0.6))
            };
            // Faint aurora-like bands drifting above the horizon.
            if elev > 0.0 {
                let n = noise(u * 24.0, elev * 10.0, 24, 7) * 0.6
                    + noise(u * 64.0, elev * 26.0, 64, 11) * 0.4;
                let band = (1.0 - ((elev - 0.18) / 0.16).abs()).max(0.0);
                let k = band * n * n * 0.10;
                c = [c[0] + k * 0.25, c[1] + k * 0.55, c[2] + k * 0.75];
            }
            // Warm glow hugging the horizon.
            let g = (-(elev.abs() / 0.045)).exp() * (0.55 + 0.45 * noise(u * 8.0, 0.0, 8, 3));
            c = mix(c, glow, g * 0.6);
            let i = (y * w + x) * 4;
            for k in 0..3 {
                out[i + k] = (to_srgb(c[k]) * 255.0).round() as u8;
            }
            out[i + 3] = 255;
        }
    }

    // Stars: denser and brighter towards the zenith, none below the horizon.
    let mut rng = Rng(0x5EED_CAFE_F00D);
    for _ in 0..9000 {
        let x = rng.unit() * w as f32;
        let elev = rng.unit().powf(0.8);
        if elev < 0.04 {
            continue;
        }
        let y = (1.0 - elev) * 0.5 * h as f32;
        // Equirect pixels get narrower towards the pole: stretch stars to stay round.
        let stretch = 1.0 / (elev * std::f32::consts::FRAC_PI_2).cos().max(0.05);
        let brightness = rng.unit().powf(6.0) * 0.9 + 0.08;
        let tint = rng.unit();
        let color = mix([0.75, 0.82, 1.0], [1.0, 0.88, 0.75], tint);
        let radius = 0.6 + brightness * 1.4;
        plot_star(
            &mut out,
            w,
            h,
            x,
            y,
            radius * stretch,
            radius,
            brightness,
            color,
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn plot_star(
    out: &mut [u8],
    w: usize,
    h: usize,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    brightness: f32,
    color: [f32; 3],
) {
    let (x0, x1) = (
        (cx - rx - 1.0).floor() as i64,
        (cx + rx + 1.0).ceil() as i64,
    );
    let (y0, y1) = (
        (cy - ry - 1.0).floor() as i64,
        (cy + ry + 1.0).ceil() as i64,
    );
    for y in y0.max(0)..=y1.min(h as i64 - 1) {
        for x in x0..=x1 {
            let dx = (x as f32 + 0.5 - cx) / rx;
            let dy = (y as f32 + 0.5 - cy) / ry;
            let a = (-(dx * dx + dy * dy) * 2.5).exp() * brightness;
            if a < 0.004 {
                continue;
            }
            let xi = x.rem_euclid(w as i64) as usize;
            let i = (y as usize * w + xi) * 4;
            for k in 0..3 {
                let lin = from_srgb(out[i + k] as f32 / 255.0) + color[k] * a;
                out[i + k] = (to_srgb(lin.min(1.0)) * 255.0).round() as u8;
            }
        }
    }
}

fn to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn from_srgb(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sky_is_opaque_and_dark_below() {
        let px = super::night_sky();
        assert_eq!(px.len(), (super::WIDTH * super::HEIGHT * 4) as usize);
        assert!(px.chunks(4).all(|p| p[3] == 255));
        let bottom = ((super::HEIGHT - 1) * super::WIDTH * 4) as usize;
        assert!(
            px[bottom] < 50 && px[bottom + 2] < 60,
            "{:?}",
            &px[bottom..bottom + 4]
        );
    }
}
