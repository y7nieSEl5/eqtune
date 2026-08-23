//! Parametric biquad EQ (RBJ "Audio EQ Cookbook" coefficients), preamp, and a
//! transparent-below-threshold soft limiter.
//!
//! Audio is processed as interleaved `f32` (typically stereo). Each channel runs its
//! own independent cascade of biquads so filter state never bleeds between channels.
//! Signal path per sample: `preamp -> biquad cascade -> optional soft limiter`.

use std::f32::consts::PI;
use std::sync::atomic::{AtomicU64, Ordering};

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

    /// Magnitude response `|H(e^{jw})|` at frequency `f` (Hz).
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

/// Combined linear-filter response at one frequency, including preamp but excluding the
/// nonlinear limiter. Analysis commands and automatic headroom use this same calculation.
pub fn response_db(bands: &[Band], preamp_db: f32, fs: f32, freq: f32) -> f32 {
    let coeffs = active_coeffs(bands, fs);
    response_from_coeffs(&coeffs, preamp_db, fs, freq)
}

/// Combined response for several frequencies, designing each band only once.
pub fn response_curve_db(bands: &[Band], preamp_db: f32, fs: f32, frequencies: &[f32]) -> Vec<f32> {
    let coeffs = active_coeffs(bands, fs);
    frequencies
        .iter()
        .map(|freq| response_from_coeffs(&coeffs, preamp_db, fs, *freq))
        .collect()
}

fn response_from_coeffs(coeffs: &[Coeffs], preamp_db: f32, fs: f32, freq: f32) -> f32 {
    coeffs.iter().fold(preamp_db, |db, coeffs| {
        db + 20.0 * coeffs.magnitude(freq, fs).log10()
    })
}

fn active_coeffs(bands: &[Band], fs: f32) -> Vec<Coeffs> {
    bands
        .iter()
        .filter(|band| band.gain_db.abs() >= IDENTITY_GAIN_EPS_DB)
        .map(|band| Coeffs::design(band, fs))
        .collect()
}

/// Sampled peak of the filter cascade (without preamp) over the audible range.
pub fn peak_response_db(bands: &[Band], fs: f32) -> f32 {
    const STEPS: usize = 4096;
    let low = 20.0f32;
    let high = 20_000.0f32.min(fs * 0.499);
    let ratio = high / low;
    let coeffs = active_coeffs(bands, fs);
    let mut peak = 0.0f32;
    for step in 0..=STEPS {
        let freq = low * ratio.powf(step as f32 / STEPS as f32);
        peak = peak.max(response_from_coeffs(&coeffs, 0.0, fs, freq));
    }
    // Explicitly include every configured center/corner, which is the likely extremum
    // for a narrow peaking band and costs nothing meaningful on a control command.
    for band in bands.iter().filter(|band| band.freq < fs * 0.5) {
        peak = peak.max(response_from_coeffs(&coeffs, 0.0, fs, band.freq));
    }
    peak
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

/// Bands within this many dB of flat are identity filters; they are dropped from the
/// realtime coefficient set (see [`EqSettings::new`]) since they would cost a biquad per
/// sample while contributing nothing audible.
const IDENTITY_GAIN_EPS_DB: f32 = 1e-3;

/// An immutable snapshot of everything the real-time processor needs: a biquad
/// coefficient set per band, a linear preamp gain, the limiter flag, and the runtime
/// bypass endpoint. Cheap to share and swapped atomically, so the control thread can
/// update the EQ live without ever locking the audio thread.
///
/// Every field is private, so the only way to obtain a value is [`EqSettings::new`],
/// which stamps a fresh `generation`. That makes the snapshot
/// genuinely immutable: content and stamp cannot drift apart, so the [`Processor`]'s
/// generation-based change detection can never miss an edit (the hazard a
/// clone-then-mutate of public fields would otherwise open).
#[derive(Clone, Debug)]
pub struct EqSettings {
    /// The source band for each coefficient, in the same order. The processor uses this
    /// metadata to distinguish an unchanged section from a different band that moved into
    /// the same vector slot after an insertion, removal, or edit.
    bands: Vec<Band>,
    coeffs: Vec<Coeffs>,
    preamp: f32,
    limiter: bool,
    bypassed: bool,
    bypass_ramp_frames: u32,
    /// Version stamp, unique per constructed snapshot ([`EqSettings::new`] draws it from a
    /// process-global counter). The real-time [`Processor`] compares it against the last
    /// snapshot it synced to decide whether to re-copy coefficients, so an update is
    /// detected by value and never by heap address — immune to an `Arc` being freed and
    /// its address reused between two audio blocks. A clone shares the stamp, which is
    /// correct precisely because the fields are private: a clone can never be mutated, so
    /// it really is the same snapshot.
    generation: u64,
}

/// Source of the [`EqSettings`] `generation` stamp: every constructed snapshot takes the
/// next value, so any two separately built settings compare unequal — uniqueness holds by
/// construction, with no re-stamping step to forget on any publishing path.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

impl EqSettings {
    /// Design coefficients for `bands` at sample rate `fs` (Hz). Bands at (essentially)
    /// 0 dB are skipped: they are mathematically identity, so omitting them saves a
    /// biquad per sample with no audible change.
    pub fn new(bands: &[Band], fs: f32, preamp_db: f32, limiter: bool) -> Self {
        Self::with_bypass(bands, fs, preamp_db, limiter, false)
    }

    /// Build a snapshot with a runtime dry-path endpoint. Bypass is deliberately absent
    /// from persisted config; it only controls the processor's click-free A/B mix.
    pub fn with_bypass(
        bands: &[Band],
        fs: f32,
        preamp_db: f32,
        limiter: bool,
        bypassed: bool,
    ) -> Self {
        let bands: Vec<Band> = bands
            .iter()
            .filter(|b| b.gain_db.abs() >= IDENTITY_GAIN_EPS_DB)
            .copied()
            .collect();
        let coeffs: Vec<Coeffs> = bands.iter().map(|b| Coeffs::design(b, fs)).collect();
        // The real-time [`Processor`] reserves `MAX_BANDS` capacity per cascade and resizes
        // to `coeffs.len()` each block without reallocating — which holds only while a
        // snapshot never carries more than `MAX_BANDS` sections. The mutation edges
        // (`SetBand`, preset import, config load) all cap band count, so this is unreachable
        // today; assert it at the single construction site anyway, so a future band-producing
        // path that skips those caps trips here in tests instead of reallocating on the audio
        // thread.
        debug_assert!(
            coeffs.len() <= MAX_BANDS,
            "EqSettings built with {} sections, exceeding MAX_BANDS ({MAX_BANDS})",
            coeffs.len()
        );
        Self {
            bands,
            coeffs,
            preamp: db_to_lin(preamp_db),
            limiter,
            bypassed,
            bypass_ramp_frames: (fs * 0.010).round().max(1.0) as u32,
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
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
/// coefficients to the supplied [`EqSettings`] and processes in place. Unchanged bands
/// retain their filter memory, but changed coefficients, preamp, and limiter state are
/// adopted at a block boundary without smoothing. Runtime bypass alone ramps between dry
/// and wet while continuing to advance filter state.
pub struct Processor {
    channels: Vec<Vec<Biquad>>,
    /// Band metadata corresponding to the current cascade slots. Capacity is reserved on
    /// construction so updating it on the audio thread cannot allocate.
    band_layout: Vec<Band>,
    /// Generation of the [`EqSettings`] last synced into the cascades, or `None` before the
    /// first block. Every constructed snapshot carries a unique generation, so "did it
    /// change?" is a value comparison — immune to an `Arc` being freed and its heap address
    /// reused between two audio blocks, which a pointer-identity check could mistake for
    /// "unchanged".
    last_generation: Option<u64>,
    /// Consecutive near-silent blocks seen so far (gates the silence-skip).
    silent_blocks: u32,
    wet_mix: f32,
    wet_target: f32,
    bypass_ramp_remaining: u32,
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
            band_layout: Vec::with_capacity(MAX_BANDS),
            last_generation: None,
            silent_blocks: 0,
            wet_mix: 1.0,
            wet_target: 1.0,
            bypass_ramp_remaining: 0,
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
            let next_wet = if settings.bypassed { 0.0 } else { 1.0 };
            if self.last_generation.is_none() {
                // Startup adopts the requested endpoint directly; there is no preceding
                // audible eqtune path to transition from.
                self.wet_mix = next_wet;
                self.wet_target = next_wet;
                self.bypass_ramp_remaining = 0;
            } else if next_wet != self.wet_target {
                self.wet_target = next_wet;
                self.bypass_ramp_remaining = settings.bypass_ramp_frames;
            }
            // `n <= MAX_BANDS` for any preset the mutation paths accept, and each cascade
            // was constructed with that capacity reserved, so this resize stays within
            // capacity and does not allocate on the audio thread.
            let n = settings.coeffs.len();
            let old_n = self.band_layout.len();
            for cascade in self.channels.iter_mut() {
                if cascade.len() != n {
                    cascade.resize(n, Biquad::new(Coeffs::identity()));
                }
                for (index, (bq, c)) in cascade.iter_mut().zip(settings.coeffs.iter()).enumerate() {
                    // Filter delay memory is meaningful only for the exact band that
                    // produced it. Reset if an insertion/removal shifted another band into
                    // this slot, or if frequency/gain/Q/kind changed in place.
                    if index >= old_n || self.band_layout[index] != settings.bands[index] {
                        bq.reset();
                    }
                    bq.set_coeffs(*c);
                }
            }
            self.band_layout.resize(
                n,
                Band {
                    kind: BandKind::Peaking,
                    freq: 20.0,
                    gain_db: 0.0,
                    q: 1.0,
                },
            );
            self.band_layout.copy_from_slice(&settings.bands);
            self.last_generation = Some(settings.generation);
        }

        // Skip the per-sample EQ on sustained silence. Before entering the skip state,
        // reset every section: merely freezing its delay elements would preserve an old
        // filter tail and inject it when audio resumes. Near-silent input is zeroed while
        // skipped so bypassing the preamp/filter path cannot leak a different signal.
        if silent {
            self.silent_blocks = self.silent_blocks.saturating_add(1);
            if self.silent_blocks > SILENCE_SKIP_BLOCKS {
                if self.silent_blocks == SILENCE_SKIP_BLOCKS + 1 {
                    for cascade in &mut self.channels {
                        for bq in cascade {
                            bq.reset();
                        }
                    }
                }
                buf.fill(0.0);
                return silent;
            }
        } else {
            self.silent_blocks = 0;
        }

        let frames = buf.len() / channels;
        let active = channels.min(self.channels.len());
        for frame in 0..frames {
            if self.bypass_ramp_remaining > 0 {
                self.wet_mix +=
                    (self.wet_target - self.wet_mix) / self.bypass_ramp_remaining as f32;
                self.bypass_ramp_remaining -= 1;
            }
            for ch in 0..active {
                let idx = frame * channels + ch;
                let dry = buf[idx];
                let mut s = dry * settings.preamp;
                for bq in self.channels[ch].iter_mut() {
                    s = bq.process(s);
                }
                if settings.limiter {
                    s = soft_clip(s);
                }
                if self.wet_mix == 1.0 {
                    buf[idx] = s;
                } else if self.wet_mix > 0.0 {
                    buf[idx] = dry + (s - dry) * self.wet_mix;
                }
            }
        }
        silent
    }
}

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
    fn shared_response_combines_preamp_and_bands() {
        let fs = 48_000.0;
        let bands = [peak(1_000.0), peak(1_000.0)];
        let got = response_db(&bands, -3.0, fs, 1_000.0);
        assert!((got - 9.0).abs() < 0.2, "combined response was {got} dB");
    }

    #[test]
    fn shared_response_drops_the_same_identity_bands_as_realtime() {
        let nearly_flat = Band {
            kind: BandKind::Peaking,
            freq: 1_000.0,
            gain_db: IDENTITY_GAIN_EPS_DB / 2.0,
            q: 10.0,
        };
        assert_eq!(response_db(&[nearly_flat], -3.0, 48_000.0, 1_000.0), -3.0);
    }

    #[test]
    fn peak_response_finds_a_narrow_band() {
        let band = Band {
            kind: BandKind::Peaking,
            freq: 1_337.0,
            gain_db: 12.0,
            q: 10.0,
        };
        let peak = peak_response_db(&[band], 48_000.0);
        assert!((peak - 12.0).abs() < 0.1, "peak was {peak} dB");
    }

    #[test]
    fn processor_applies_and_handles_band_count_change() {
        let mut p = Processor::new(2);
        let s2 = EqSettings::new(&[peak(500.0), peak(2_000.0)], 48_000.0, 3.0, true);
        let mut buf = vec![0.3f32; 512 * 2];
        p.run(&s2, &mut buf, 2);
        assert!(buf.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
        // Shrink to one band — the cascade must resize without panicking. Every
        // constructed snapshot carries a distinct generation, so the audio thread picks
        // up the new coefficient count.
        let s1 = EqSettings::new(
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
        p.run(&s1, &mut buf, 2);
        assert!(buf.iter().all(|x| x.is_finite()));
        assert_eq!(
            p.channels[0].len(),
            1,
            "new snapshot must resize the cascade"
        );
    }

    #[test]
    fn initial_bypass_is_bit_exact_dry() {
        let mut processor = Processor::new(2);
        let settings = EqSettings::with_bypass(&[peak(1_000.0)], 48_000.0, 6.0, true, true);
        let original = vec![0.25f32; 128 * 2];
        let mut buffer = original.clone();
        processor.run(&settings, &mut buffer, 2);
        assert_eq!(buffer, original);
    }

    #[test]
    fn bypass_ramps_and_keeps_filter_state_warm() {
        let band = Band {
            kind: BandKind::Peaking,
            freq: 100.0,
            gain_db: 12.0,
            q: 5.0,
        };
        let mut processor = Processor::new(1);
        let wet = EqSettings::with_bypass(&[band], 48_000.0, 0.0, false, false);
        let dry = EqSettings::with_bypass(&[band], 48_000.0, 0.0, false, true);
        let mut signal = vec![0.25f32; 64];
        processor.run(&wet, &mut signal, 1);
        processor.run(&dry, &mut signal[..1], 1);
        assert!(processor.wet_mix > 0.0 && processor.wet_mix < 1.0);

        let mut impulse = vec![0.0f32; 480];
        impulse[0] = 1.0;
        processor.run(&dry, &mut impulse, 1);
        assert_eq!(processor.wet_mix, 0.0);
        assert_eq!(impulse[479], 0.0, "fully bypassed output must be dry");

        let wet_again = EqSettings::with_bypass(&[band], 48_000.0, 0.0, false, false);
        let mut silence = vec![0.0f32; 1];
        processor.run(&wet_again, &mut silence, 1);
        assert_ne!(
            silence[0], 0.0,
            "the dry interval must still advance filter state"
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
        let layout_cap_before = p.band_layout.capacity();
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
        assert_eq!(layout_cap_before, p.band_layout.capacity());
    }

    #[test]
    fn changed_or_shifted_bands_do_not_inherit_filter_memory() {
        let mut p = Processor::new(1);
        let original = EqSettings::new(&[peak(1_000.0), peak(5_000.0)], 48_000.0, 0.0, false);
        let mut impulse = vec![0.0f32; 32];
        impulse[0] = 1.0;
        p.run(&original, &mut impulse, 1);
        assert!(p.channels[0].iter().any(|bq| bq.z1 != 0.0 || bq.z2 != 0.0));

        // Inserting ahead of both existing sections changes every vector position. A zero
        // block after the update must remain exactly zero rather than emitting state that
        // belonged to the old occupants of those positions.
        let inserted = EqSettings::new(
            &[peak(200.0), peak(1_000.0), peak(5_000.0)],
            48_000.0,
            0.0,
            false,
        );
        let mut silence = vec![0.0f32; 32];
        p.run(&inserted, &mut silence, 1);
        assert!(silence.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn sustained_silence_skips_then_resumes() {
        let mut p = Processor::new(2);
        let s = EqSettings::new(&[peak(1_000.0)], 48_000.0, 3.0, true);

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
    fn entering_silence_skip_clears_filter_memory() {
        let mut p = Processor::new(1);
        let s = EqSettings::new(&[peak(20.0)], 48_000.0, 0.0, false);

        // Excite a slow, low-frequency section so three zero blocks are not enough for its
        // internal delay elements to decay naturally to exact zero.
        let mut impulse = vec![0.0f32; 64];
        impulse[0] = 1.0;
        p.run(&s, &mut impulse, 1);
        for _ in 0..=SILENCE_SKIP_BLOCKS {
            let mut silence = vec![0.0f32; 64];
            p.run(&s, &mut silence, 1);
        }

        assert!(
            p.channels
                .iter()
                .flatten()
                .all(|bq| bq.z1 == 0.0 && bq.z2 == 0.0),
            "the first skipped block must clear stale filter state"
        );
    }

    #[test]
    fn run_reports_input_silence() {
        // The audio callback drives idle detection off this return value, so it must
        // reflect the block's input regardless of how the block is processed.
        let mut p = Processor::new(2);
        let s = EqSettings::new(&[peak(1_000.0)], 48_000.0, 3.0, true);
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

        // A distinct generation is adopted, resizing the cascade. A freshly constructed
        // snapshot is distinct without any manual stamping — the PR #1 regression where
        // `EqSettings::new` left `generation == 0` on every snapshot made a `Processor`
        // driven directly through the public API (no `EqHandle::store` in between) treat this
        // second value as unchanged and keep the previous coefficients.
        let one_fresh = EqSettings::new(&[peak(1000.0)], 48_000.0, 0.0, false);
        p.run(&one_fresh, &mut buf, 1);
        assert_eq!(
            p.channels[0].len(),
            1,
            "a distinct generation must resync to one section"
        );
    }
}
