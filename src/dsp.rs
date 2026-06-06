//! Parametric biquad EQ (RBJ "Audio EQ Cookbook" coefficients), preamp, and a
//! transparent-below-threshold soft limiter.
//!
//! Audio is processed as interleaved `f32` (typically stereo). Each channel runs its
//! own independent cascade of biquads so filter state never bleeds between channels.
//! Signal path per sample: `preamp -> biquad cascade -> optional soft limiter`.

use std::f32::consts::PI;

use serde::{Deserialize, Serialize};

/// The kind of filter a [`Band`] represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandKind {
    Peaking,
    LowShelf,
    HighShelf,
}

/// One parametric band: filter kind, center/corner frequency (Hz), gain (dB), and Q.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub kind: BandKind,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// Normalized biquad coefficients (a0 has been divided out, so a0 == 1).
#[derive(Clone, Copy, Debug)]
pub struct Coeffs {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Coeffs {
    /// Pass-through filter (unity at all frequencies).
    pub fn identity() -> Self {
        Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }
    }

    /// RBJ cookbook coefficients for `band` at sample rate `fs` (Hz).
    pub fn design(band: &Band, fs: f32) -> Self {
        match band.kind {
            BandKind::Peaking => Self::peaking(fs, band.freq, band.gain_db, band.q),
            BandKind::LowShelf => Self::low_shelf(fs, band.freq, band.gain_db, band.q),
            BandKind::HighShelf => Self::high_shelf(fs, band.freq, band.gain_db, band.q),
        }
    }

    fn peaking(fs: f32, f0: f32, gain_db: f32, q: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha / a;
        Self::normalized(b0, b1, b2, a0, a1, a2)
    }

    fn low_shelf(fs: f32, f0: f32, gain_db: f32, q: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q);
        let beta = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) - (a - 1.0) * cos + beta);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos - beta);
        let a0 = (a + 1.0) + (a - 1.0) * cos + beta;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos);
        let a2 = (a + 1.0) + (a - 1.0) * cos - beta;
        Self::normalized(b0, b1, b2, a0, a1, a2)
    }

    fn high_shelf(fs: f32, f0: f32, gain_db: f32, q: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q);
        let beta = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) + (a - 1.0) * cos + beta);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos - beta);
        let a0 = (a + 1.0) - (a - 1.0) * cos + beta;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
        let a2 = (a + 1.0) - (a - 1.0) * cos - beta;
        Self::normalized(b0, b1, b2, a0, a1, a2)
    }

    fn normalized(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// Magnitude response `|H(e^{jw})|` at frequency `f` (Hz). Used by tests and any
    /// future spectrum/preview tooling.
    pub fn magnitude(&self, f: f32, fs: f32) -> f32 {
        let w = 2.0 * PI * f / fs;
        // e^{-jw} = cos(w) - j sin(w); e^{-2jw} = cos(2w) - j sin(2w)
        let (cw, sw) = (w.cos(), w.sin());
        let (c2w, s2w) = ((2.0 * w).cos(), (2.0 * w).sin());
        let num_re = self.b0 + self.b1 * cw + self.b2 * c2w;
        let num_im = -(self.b1 * sw + self.b2 * s2w);
        let den_re = 1.0 + self.a1 * cw + self.a2 * c2w;
        let den_im = -(self.a1 * sw + self.a2 * s2w);
        let num = (num_re * num_re + num_im * num_im).sqrt();
        let den = (den_re * den_re + den_im * den_im).sqrt();
        num / den
    }
}

/// A single biquad section using Transposed Direct Form II (good float behavior).
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    coeffs: Coeffs,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn new(coeffs: Coeffs) -> Self {
        Self { coeffs, z1: 0.0, z2: 0.0 }
    }

    pub fn set_coeffs(&mut self, coeffs: Coeffs) {
        self.coeffs = coeffs;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let c = &self.coeffs;
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// dB → linear amplitude.
#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Transparent-below-threshold soft limiter: identity for `|x| <= T`, then a smooth
/// knee that asymptotically approaches ±1 so the preamp can never hard-clip.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    const T: f32 = 0.9;
    let a = x.abs();
    if a <= T {
        x
    } else {
        let over = (a - T) / (1.0 - T); // >= 0
        let shaped = T + (1.0 - T) * (over / (1.0 + over)); // -> 1 as over -> inf
        shaped.copysign(x)
    }
}

/// Full equalizer: preamp gain, a per-channel biquad cascade, and an optional limiter.
pub struct Equalizer {
    fs: f32,
    preamp: f32, // linear
    limiter: bool,
    bands: Vec<Band>,
    channels: Vec<Vec<Biquad>>, // one cascade per channel
}

impl Equalizer {
    pub fn new(fs: f32, channels: usize, bands: Vec<Band>, preamp_db: f32, limiter: bool) -> Self {
        let mut eq = Self {
            fs,
            preamp: db_to_lin(preamp_db),
            limiter,
            bands: Vec::new(),
            channels: vec![Vec::new(); channels],
        };
        eq.set_bands(bands);
        eq
    }

    /// Rebuild every channel's cascade from `bands`, preserving filter state where the
    /// cascade length is unchanged (so live edits don't click).
    pub fn set_bands(&mut self, bands: Vec<Band>) {
        let coeffs: Vec<Coeffs> = bands.iter().map(|b| Coeffs::design(b, self.fs)).collect();
        for ch in self.channels.iter_mut() {
            ch.resize(coeffs.len(), Biquad::new(Coeffs::identity()));
            for (bq, c) in ch.iter_mut().zip(coeffs.iter()) {
                bq.set_coeffs(*c);
            }
        }
        self.bands = bands;
    }

    pub fn set_preamp_db(&mut self, db: f32) {
        self.preamp = db_to_lin(db);
    }

    pub fn set_limiter(&mut self, on: bool) {
        self.limiter = on;
    }

    /// Re-design all filters for a new sample rate and clear state.
    pub fn set_sample_rate(&mut self, fs: f32) {
        self.fs = fs;
        let bands = std::mem::take(&mut self.bands);
        self.set_bands(bands);
        for ch in self.channels.iter_mut() {
            for bq in ch.iter_mut() {
                bq.reset();
            }
        }
    }

    pub fn bands(&self) -> &[Band] {
        &self.bands
    }

    /// Process an interleaved buffer in place. `channels` is the interleave stride and
    /// must be `<=` the channel count this equalizer was built with.
    pub fn process_interleaved(&mut self, buf: &mut [f32], channels: usize) {
        debug_assert!(channels <= self.channels.len());
        let frames = buf.len() / channels;
        for frame in 0..frames {
            for ch in 0..channels {
                let idx = frame * channels + ch;
                let mut s = buf[idx] * self.preamp;
                for bq in self.channels[ch].iter_mut() {
                    s = bq.process(s);
                }
                if self.limiter {
                    s = soft_clip(s);
                }
                buf[idx] = s;
            }
        }
    }
}

/// The built-in "default" curve — a 9-band, graphic-EQ-style tuning from the user:
/// a broad ~-5 dB low/low-mid cut, a scoop through 1-2 kHz to tame harsh mids, a small
/// lift of air up top, with +7 dB make-up gain ([`DEFAULT_PREAMP_DB`]).
///
/// Modeled as peaking filters at ~octave Q (the conventional graphic-EQ shape); pure
/// data, tunable live via `eqtune band`.
pub fn default_bands() -> Vec<Band> {
    const Q: f32 = 1.41;
    [
        (32.0, -5.0),
        (64.0, -5.0),
        (125.0, -5.0),
        (500.0, -5.0),
        (1_000.0, -10.0),
        (2_000.0, -15.0),
        (4_000.0, -4.0),
        (8_000.0, 2.0),
        (16_000.0, 0.0),
    ]
    .into_iter()
    .map(|(freq, gain_db)| Band { kind: BandKind::Peaking, freq, gain_db, q: Q })
    .collect()
}

/// Default make-up gain that pairs with [`default_bands`].
pub const DEFAULT_PREAMP_DB: f32 = 7.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn db(mag: f32) -> f32 {
        20.0 * mag.log10()
    }

    #[test]
    fn identity_is_flat() {
        let c = Coeffs::identity();
        for f in [20.0, 500.0, 5_000.0, 18_000.0] {
            assert!((c.magnitude(f, 48_000.0) - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn peaking_center_gain_matches_design() {
        let fs = 48_000.0;
        for gain in [-25.0, -10.0, -5.0, 6.0, 12.0] {
            let band = Band { kind: BandKind::Peaking, freq: 1000.0, gain_db: gain, q: 1.0 };
            let c = Coeffs::design(&band, fs);
            let got = db(c.magnitude(1000.0, fs));
            assert!((got - gain).abs() < 0.1, "design {gain} dB, got {got} dB");
        }
    }

    #[test]
    fn peaking_is_unity_far_from_center() {
        let fs = 48_000.0;
        let band = Band { kind: BandKind::Peaking, freq: 1000.0, gain_db: -25.0, q: 1.0 };
        let c = Coeffs::design(&band, fs);
        assert!(db(c.magnitude(60.0, fs)).abs() < 1.0);
        assert!(db(c.magnitude(16_000.0, fs)).abs() < 1.0);
    }

    #[test]
    fn low_shelf_dc_and_nyquist() {
        let fs = 48_000.0;
        let band = Band { kind: BandKind::LowShelf, freq: 110.0, gain_db: -5.0, q: 0.7 };
        let c = Coeffs::design(&band, fs);
        assert!((db(c.magnitude(5.0, fs)) - (-5.0)).abs() < 0.5, "dc shelf");
        assert!(db(c.magnitude(20_000.0, fs)).abs() < 0.5, "near nyquist flat");
    }

    #[test]
    fn soft_clip_is_transparent_then_bounded() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.5), -0.5);
        for x in [1.0, 2.0, 50.0, -50.0] {
            assert!(soft_clip(x).abs() < 1.0);
        }
    }

    #[test]
    fn process_is_finite_and_bounded_with_default_curve() {
        let mut eq = Equalizer::new(48_000.0, 2, default_bands(), DEFAULT_PREAMP_DB, true);
        let mut buf = vec![0.0f32; 4096 * 2];
        for (i, s) in buf.iter_mut().enumerate() {
            *s = (i as f32 * 0.1).sin() * 0.8; // loud-ish interleaved stereo
        }
        eq.process_interleaved(&mut buf, 2);
        assert!(buf.iter().all(|x| x.is_finite()));
        assert!(buf.iter().all(|x| x.abs() <= 1.0));
    }

    #[test]
    fn live_band_edit_preserves_cascade() {
        let mut eq = Equalizer::new(44_100.0, 2, default_bands(), 0.0, false);
        assert_eq!(eq.bands().len(), default_bands().len());
        eq.set_bands(vec![Band { kind: BandKind::Peaking, freq: 3000.0, gain_db: 4.0, q: 2.0 }]);
        assert_eq!(eq.bands().len(), 1);
        let mut buf = vec![0.25f32; 256 * 2];
        eq.process_interleaved(&mut buf, 2); // must not panic on resized cascade
        assert!(buf.iter().all(|x| x.is_finite()));
    }
}
