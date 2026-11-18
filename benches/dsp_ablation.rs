//! Offline, leave-one-component-out ablation using the shipped presets.
//! Notes and recorded results: experiments/dsp-ablation/README.md.
//! Run: cargo bench --locked --bench dsp_ablation > /tmp/eqtune-dsp-ablation.csv
use std::hint::black_box;
use std::time::Instant;

use eqtune::config::Config;
use eqtune::dsp::{EqSettings, Processor};

const FS: f32 = 48_000.0;
const CHANNELS: usize = 2;
const FRAMES: usize = 256;
const SAMPLES: usize = FRAMES * CHANNELS;
const BLOCKS: usize = 16;
const WARMUP: usize = 256;
const ITERATIONS: usize = 4096;
const ROUNDS: usize = 7;

fn input(amplitude: f32) -> Vec<[f32; SAMPLES]> {
    // A continuous, periodic stereo multitone. All frequencies align with the
    // 4096-frame period; normalization bounds the input peak by amplitude.
    (0..BLOCKS)
        .map(|block| {
            std::array::from_fn(|i| {
                let frame = block * FRAMES + i / CHANNELS;
                let phase = if i % CHANNELS == 0 { 0.3 } else { 1.1 };
                let sum: f32 = [5.0, 43.0, 171.0, 683.0]
                    .iter()
                    .enumerate()
                    .map(|(index, bin)| {
                        (std::f32::consts::TAU * bin * frame as f32 / (BLOCKS * FRAMES) as f32
                            + phase * (index + 1) as f32)
                            .sin()
                    })
                    .sum();
                amplitude * sum / 4.0
            })
        })
        .collect()
}

fn warm(processor: &mut Processor, settings: &EqSettings, inputs: &[[f32; SAMPLES]]) {
    for i in 0..WARMUP {
        let mut buffer = inputs[i % BLOCKS];
        processor.run(settings, &mut buffer, CHANNELS);
    }
}

fn measure(settings: &EqSettings, inputs: &[[f32; SAMPLES]]) -> f64 {
    let mut processor = Processor::new(CHANNELS);
    warm(&mut processor, settings, inputs);
    let start = Instant::now();
    for i in 0..ITERATIONS {
        // Restore untouched input each time: never feed processed output back in.
        // The same copy and black_box overhead applies to every variant.
        let mut buffer = black_box(inputs[i % BLOCKS]);
        black_box(processor.run(black_box(settings), &mut buffer, CHANNELS));
        black_box(buffer);
    }
    start.elapsed().as_secs_f64() * 1e9 / ITERATIONS as f64
}

fn render(settings: &EqSettings, inputs: &[[f32; SAMPLES]]) -> Vec<f32> {
    let mut processor = Processor::new(CHANNELS);
    warm(&mut processor, settings, inputs);
    let mut output = Vec::with_capacity(BLOCKS * SAMPLES);
    for input in inputs {
        let mut buffer = *input;
        processor.run(settings, &mut buffer, CHANNELS);
        assert!(buffer.iter().all(|sample| sample.is_finite()));
        output.extend(buffer);
    }
    output
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("run with cargo bench --bench dsp_ablation for optimized measurements");
        return;
    }
    let config = Config::default();
    println!(
        "preset,input,variant,round,ns_per_block,peak,rms,rms_delta_from_full,samples_above_unity"
    );
    for preset_name in ["bright", "mellow", "pro"] {
        let preset = &config.presets[preset_name];
        let variants = [
            (
                "full",
                EqSettings::new(&preset.bands, FS, preset.preamp_db, true),
            ),
            ("no_eq", EqSettings::new(&[], FS, preset.preamp_db, true)),
            (
                "unity_preamp",
                EqSettings::new(&preset.bands, FS, 0.0, true),
            ),
            (
                "no_limiter",
                EqSettings::new(&preset.bands, FS, preset.preamp_db, false),
            ),
            (
                "dry_bypass",
                EqSettings::with_bypass(&preset.bands, FS, preset.preamp_db, true, true),
            ),
        ];
        // Both nonzero workloads are bounded below full scale on input. Loud
        // input stresses the boost/headroom interaction when preamp is ablated.
        for (input_name, amplitude) in [("normal", 0.25), ("loud", 1.0), ("silence", 0.0)] {
            let inputs = input(amplitude);
            let reference = render(&variants[0].1, &inputs);
            let metrics: Vec<_> = variants
                .iter()
                .map(|(name, settings)| {
                    let output = render(settings, &inputs);
                    let peak = output.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
                    let rms = (output.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>()
                        / output.len() as f64)
                        .sqrt();
                    let delta = (output
                        .iter()
                        .zip(&reference)
                        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
                        .sum::<f64>()
                        / output.len() as f64)
                        .sqrt();
                    let over = output.iter().filter(|x| x.abs() > 1.0).count();
                    if *name != "no_limiter" {
                        assert!(peak <= 1.0);
                    }
                    if input_name == "silence" {
                        assert_eq!(peak, 0.0);
                    }
                    if *name == "dry_bypass" {
                        assert!(output.iter().copied().eq(inputs.iter().flatten().copied()));
                    }
                    (peak, rms, delta, over)
                })
                .collect();
            // Rotate and reverse order across rounds to reduce order bias.
            for round in 0..ROUNDS {
                for slot in 0..variants.len() {
                    let offset = if round % 2 == 0 {
                        slot
                    } else {
                        variants.len() - 1 - slot
                    };
                    let index = (offset + round) % variants.len();
                    let (name, settings) = &variants[index];
                    let ns = measure(settings, &inputs);
                    let (peak, rms, delta, over) = metrics[index];
                    println!(
                        "{preset_name},{input_name},{name},{round},{ns:.3},{peak:.8},{rms:.8},{delta:.8},{over}"
                    );
                }
            }
        }
    }
}
