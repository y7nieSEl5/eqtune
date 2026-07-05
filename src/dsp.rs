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
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
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
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
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
        Self {
            coeffs,
            z1: 0.0,
            z2: 0.0,
        }
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

/// Bands within this many dB of flat are identity filters; they are dropped from the
/// realtime coefficient set (see [`EqSettings::new`]) since they would cost a biquad per
/// sample while contributing nothing audible.
const IDENTITY_GAIN_EPS_DB: f32 = 1e-3;

/// An immutable snapshot of everything the real-time processor needs: a biquad
/// coefficient set per band, a linear preamp gain, and the limiter flag. Cheap to
/// share and swapped atomically, so the control thread can update the EQ live without
/// ever locking the audio thread.
#[derive(Clone, Debug)]
pub struct EqSettings {
    pub coeffs: Vec<Coeffs>,
    pub preamp: f32,
    pub limiter: bool,
    /// Monotonic version stamp, unique per published snapshot. The real-time [`Processor`]
    /// compares it against the last snapshot it synced to decide whether to re-copy
    /// coefficients, so an update is detected by value and never by heap address — immune
    /// to an `Arc` being freed and its address reused between two audio blocks. `0` for the
    /// initial snapshot; the control thread stamps increasing values on each live update.
    pub generation: u64,
}

impl EqSettings {
    /// Design coefficients for `bands` at sample rate `fs` (Hz). Bands at (essentially)
    /// 0 dB are skipped: they are mathematically identity, so omitting them saves a
    /// biquad per sample with no audible change.
    pub fn new(bands: &[Band], fs: f32, preamp_db: f32, limiter: bool) -> Self {
        Self {
            coeffs: bands
                .iter()
                .filter(|b| b.gain_db.abs() >= IDENTITY_GAIN_EPS_DB)
                .map(|b| Coeffs::design(b, fs))
                .collect(),
            preamp: db_to_lin(preamp_db),
            limiter,
            generation: 0,
        }
    }
}

/// Upper bound on the number of biquad sections per channel the real-time [`Processor`]
/// will ever hold. Each cascade reserves this capacity at construction (on the control
/// thread), so syncing a new coefficient set on the audio thread never reallocates —
/// upholding the "audio thread never allocates" invariant by construction. Band-adding
/// paths (`SetBand`, preset import) reject anything beyond this, so a normal preset can
/// never exceed the reserved capacity. Well above the densest standard graphic EQ
/// (31-band ISO 1/3-octave).
pub const MAX_BANDS: usize = 64;

/// A block whose samples are all quieter than this (linear, ≈ −80 dBFS) counts as
/// silent for the [`Processor`]'s silence-skip optimization.
const SILENCE_THRESHOLD: f32 = 1e-4;
/// The EQ is skipped only after this many consecutive silent blocks, so a hard cut to
/// silence still renders the biquads' ring-out before we stop touching the buffer.
const SILENCE_SKIP_BLOCKS: u32 = 3;

/// Whether every sample in `buf` is below [`SILENCE_THRESHOLD`] (short-circuits on the
/// first audible sample, so it is cheap on real audio).
#[inline]
fn block_is_silent(buf: &[f32]) -> bool {
    buf.iter().all(|s| s.abs() < SILENCE_THRESHOLD)
}

/// Real-time, audio-thread-local filter state. Each block it syncs its biquad
/// coefficients to the supplied [`EqSettings`] (filter memory persists across updates
/// of the same band count, so live edits don't click) and processes in place.
pub struct Processor {
    channels: Vec<Vec<Biquad>>,
    /// Generation of the [`EqSettings`] last synced into the cascades, or `None` before the
    /// first block. The control thread stamps a fresh, monotonically increasing generation
    /// on every published snapshot, so "did it change?" is a value comparison — immune to
    /// an `Arc` being freed and its heap address reused between two audio blocks, which a
    /// pointer-identity check could mistake for "unchanged".
    last_generation: Option<u64>,
    /// Consecutive near-silent blocks seen so far (gates the silence-skip).
    silent_blocks: u32,
}

impl Processor {
    pub fn new(channels: usize) -> Self {
        Self {
            // Reserve MAX_BANDS up front (on the control thread) so the per-block
            // coefficient sync in `run` only ever resizes *within* capacity and never
            // reallocates on the audio thread.
            channels: (0..channels)
                .map(|_| Vec::with_capacity(MAX_BANDS))
                .collect(),
            last_generation: None,
            silent_blocks: 0,
        }
    }

    /// Sync coefficients if `settings` changed, EQ the block in place, and return whether
    /// the block's *input* was silent — scanned once here so the audio callback can drive
    /// idle detection without walking the buffer a second time.
    pub fn run(&mut self, settings: &EqSettings, buf: &mut [f32], channels: usize) -> bool {
        // One pass over the untouched input; reused for the silence-skip below and handed
        // back to the caller.
        let silent = block_is_silent(buf);
        if channels == 0 {
            return silent;
        }
        // Re-copy biquad coefficients only when a newer snapshot arrives, compared by
        // generation (see `last_generation`); in steady state this skips dozens of copies
        // per block.
        if self.last_generation != Some(settings.generation) {
            // `n <= MAX_BANDS` for any preset the mutation paths accept, and each cascade
            // was constructed with that capacity reserved, so this resize stays within
            // capacity and does not allocate on the audio thread.
            let n = settings.coeffs.len();
            for cascade in self.channels.iter_mut() {
                if cascade.len() != n {
                    cascade.resize(n, Biquad::new(Coeffs::identity()));
                }
                for (bq, c) in cascade.iter_mut().zip(settings.coeffs.iter()) {
                    bq.set_coeffs(*c);
                }
            }
            self.last_generation = Some(settings.generation);
        }

        // Skip the per-sample EQ on sustained silence: silent in → silent out, so once any
        // filter ring-out has been rendered (after SILENCE_SKIP_BLOCKS) we can leave the
        // already-correct buffer untouched and do no per-sample work.
        if silent {
            self.silent_blocks = self.silent_blocks.saturating_add(1);
            if self.silent_blocks > SILENCE_SKIP_BLOCKS {
                return silent;
            }
        } else {
            self.silent_blocks = 0;
        }

        let frames = buf.len() / channels;
        let active = channels.min(self.channels.len());
        for frame in 0..frames {
            for ch in 0..active {
                let idx = frame * channels + ch;
                let mut s = buf[idx] * settings.preamp;
                for bq in self.channels[ch].iter_mut() {
                    s = bq.process(s);
                }
                if settings.limiter {
                    s = soft_clip(s);
                }
                buf[idx] = s;
            }
        }
        silent
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
    .map(|(freq, gain_db)| Band {
        kind: BandKind::Peaking,
        freq,
        gain_db,
        q: Q,
    })
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

    fn peak(freq: f32) -> Band {
        Band {
            kind: BandKind::Peaking,
            freq,
            gain_db: 6.0,
            q: 1.0,
        }
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
            let band = Band {
                kind: BandKind::Peaking,
                freq: 1000.0,
                gain_db: gain,
                q: 1.0,
            };
            let c = Coeffs::design(&band, fs);
            let got = db(c.magnitude(1000.0, fs));
            assert!((got - gain).abs() < 0.1, "design {gain} dB, got {got} dB");
        }
    }

    #[test]
    fn peaking_is_unity_far_from_center() {
        let fs = 48_000.0;
        let band = Band {
            kind: BandKind::Peaking,
            freq: 1000.0,
            gain_db: -25.0,
            q: 1.0,
        };
        let c = Coeffs::design(&band, fs);
        assert!(db(c.magnitude(60.0, fs)).abs() < 1.0);
        assert!(db(c.magnitude(16_000.0, fs)).abs() < 1.0);
    }

    #[test]
    fn low_shelf_dc_and_nyquist() {
        let fs = 48_000.0;
        let band = Band {
            kind: BandKind::LowShelf,
            freq: 110.0,
            gain_db: -5.0,
            q: 0.7,
        };
        let c = Coeffs::design(&band, fs);
        assert!((db(c.magnitude(5.0, fs)) - (-5.0)).abs() < 0.5, "dc shelf");
        assert!(
            db(c.magnitude(20_000.0, fs)).abs() < 0.5,
            "near nyquist flat"
        );
    }

    #[test]
    fn processor_applies_and_handles_band_count_change() {
        let mut p = Processor::new(2);
        let s9 = EqSettings::new(&default_bands(), 48_000.0, DEFAULT_PREAMP_DB, true);
        let mut buf = vec![0.3f32; 512 * 2];
        p.run(&s9, &mut buf, 2);
        assert!(buf.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
        // Shrink to one band — the cascade must resize without panicking. A distinct
        // published snapshot carries a distinct generation (as `EqHandle::store` stamps),
        // so the audio thread picks up the new coefficient count.
        let mut s1 = EqSettings::new(
            &[Band {
                kind: BandKind::Peaking,
                freq: 3000.0,
                gain_db: 4.0,
                q: 2.0,
            }],
            48_000.0,
            0.0,
            false,
        );
        s1.generation = 1;
        p.run(&s1, &mut buf, 2);
        assert!(buf.iter().all(|x| x.is_finite()));
        assert_eq!(
            p.channels[0].len(),
            1,
            "new snapshot must resize the cascade"
        );
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
        eq.set_bands(vec![Band {
            kind: BandKind::Peaking,
            freq: 3000.0,
            gain_db: 4.0,
            q: 2.0,
        }]);
        assert_eq!(eq.bands().len(), 1);
        let mut buf = vec![0.25f32; 256 * 2];
        eq.process_interleaved(&mut buf, 2); // must not panic on resized cascade
        assert!(buf.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn zero_db_bands_are_dropped_from_coeffs() {
        let bands = vec![
            Band {
                kind: BandKind::Peaking,
                freq: 100.0,
                gain_db: 0.0,
                q: 1.0,
            }, // identity -> dropped
            Band {
                kind: BandKind::Peaking,
                freq: 1000.0,
                gain_db: 4.0,
                q: 1.0,
            }, // kept
            Band {
                kind: BandKind::LowShelf,
                freq: 80.0,
                gain_db: 0.0,
                q: 0.7,
            }, // identity -> dropped
        ];
        let s = EqSettings::new(&bands, 48_000.0, 0.0, false);
        assert_eq!(
            s.coeffs.len(),
            1,
            "only the non-zero-gain band should produce a coefficient"
        );
    }

    #[test]
    fn processor_reserves_capacity_so_run_never_reallocates() {
        let mut p = Processor::new(2);
        let cap_before: Vec<usize> = p.channels.iter().map(|c| c.capacity()).collect();
        assert!(
            cap_before.iter().all(|&c| c >= MAX_BANDS),
            "each cascade must reserve MAX_BANDS up front"
        );
        // A full MAX_BANDS worth of non-identity bands (none dropped at design time).
        let bands: Vec<Band> = (0..MAX_BANDS)
            .map(|i| Band {
                kind: BandKind::Peaking,
                freq: 100.0 + i as f32 * 100.0,
                gain_db: 3.0,
                q: 1.0,
            })
            .collect();
        let s = EqSettings::new(&bands, 48_000.0, 0.0, false);
        assert_eq!(s.coeffs.len(), MAX_BANDS);
        let mut buf = vec![0.2f32; 128 * 2];
        p.run(&s, &mut buf, 2);
        let cap_after: Vec<usize> = p.channels.iter().map(|c| c.capacity()).collect();
        assert_eq!(
            cap_before, cap_after,
            "syncing coefficients must not reallocate the audio-thread cascades"
        );
    }

    #[test]
    fn sustained_silence_skips_then_resumes() {
        let mut p = Processor::new(2);
        let s = EqSettings::new(&default_bands(), 48_000.0, DEFAULT_PREAMP_DB, true);

        // Past the skip threshold: silence stays silent (and the skip path is exercised).
        for _ in 0..(SILENCE_SKIP_BLOCKS + 5) {
            let mut buf = vec![0.0f32; 256 * 2];
            p.run(&s, &mut buf, 2);
            assert!(
                buf.iter().all(|x| *x == 0.0),
                "silent input must stay silent"
            );
        }

        // Audio after silence must resume processing (preamp/EQ changes the samples).
        let mut buf = vec![0.5f32; 256 * 2];
        p.run(&s, &mut buf, 2);
        assert!(
            buf.iter().any(|x| (*x - 0.5).abs() > 1e-6),
            "audio after silence must be EQ'd"
        );
        assert!(buf.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn run_reports_input_silence() {
        // The audio callback drives idle detection off this return value, so it must
        // reflect the block's input regardless of how the block is processed.
        let mut p = Processor::new(2);
        let s = EqSettings::new(&default_bands(), 48_000.0, DEFAULT_PREAMP_DB, true);
        let mut quiet = vec![0.0f32; 256 * 2];
        assert!(
            p.run(&s, &mut quiet, 2),
            "all-zero input must report silent"
        );
        let mut loud = vec![0.5f32; 256 * 2];
        assert!(
            !p.run(&s, &mut loud, 2),
            "audible input must report not silent"
        );
    }

    #[test]
    fn run_resyncs_cascade_only_on_new_generation() {
        let mut p = Processor::new(1);
        let three = EqSettings::new(
            &[peak(200.0), peak(1000.0), peak(5000.0)],
            48_000.0,
            0.0,
            false,
        );
        let mut buf = vec![0.1f32; 128];
        p.run(&three, &mut buf, 1);
        assert_eq!(
            p.channels[0].len(),
            3,
            "first snapshot syncs three sections"
        );

        // Same generation, fewer bands: this is exactly the freed-and-reused-address hazard
        // the generation stamp closes — the audio thread must treat it as unchanged and NOT
        // adopt the new coefficient count.
        let mut one_stale = EqSettings::new(&[peak(1000.0)], 48_000.0, 0.0, false);
        one_stale.generation = three.generation;
        p.run(&one_stale, &mut buf, 1);
        assert_eq!(
            p.channels[0].len(),
            3,
            "a snapshot with the same generation must be skipped"
        );

        // A newer generation is adopted, resizing the cascade.
        let mut one_fresh = EqSettings::new(&[peak(1000.0)], 48_000.0, 0.0, false);
        one_fresh.generation = three.generation + 1;
        p.run(&one_fresh, &mut buf, 1);
        assert_eq!(
            p.channels[0].len(),
            1,
            "a newer generation must resync to one section"
        );
    }
}
