use std::hint::black_box;
use std::time::Instant;

use eqtune::dsp::{Band, BandKind, EqSettings, MAX_BANDS, Processor};

const SAMPLE_RATE: f32 = 48_000.0;
const CHANNELS: usize = 2;
const FRAMES: usize = 256;

fn main() {
    if cfg!(debug_assertions) {
        return;
    }

    let active_bands = bands(8);
    let steady = EqSettings::new(&active_bands, SAMPLE_RATE, -3.0, true);
    let mut steady_processor = Processor::new(CHANNELS);
    let mut steady_buffer = signal(FRAMES * CHANNELS);
    bench("steady-state", 100_000, || {
        steady_buffer[0] = 0.25;
        black_box(steady_processor.run(&steady, &mut steady_buffer, CHANNELS));
    });

    let mut silence_processor = Processor::new(CHANNELS);
    let mut silence = vec![0.0; FRAMES * CHANNELS];
    for _ in 0..8 {
        silence_processor.run(&steady, &mut silence, CHANNELS);
    }
    bench("sustained-silence", 200_000, || {
        black_box(silence_processor.run(&steady, &mut silence, CHANNELS));
    });

    let changed_bands = active_bands
        .iter()
        .map(|band| Band {
            gain_db: -band.gain_db,
            ..*band
        })
        .collect::<Vec<_>>();
    let changed = EqSettings::new(&changed_bands, SAMPLE_RATE, -3.0, true);
    let mut update_processor = Processor::new(CHANNELS);
    let mut update_buffer = signal(FRAMES * CHANNELS);
    let mut use_changed = false;
    bench("settings-update", 50_000, || {
        update_buffer[0] = 0.25;
        use_changed = !use_changed;
        let settings = if use_changed { &changed } else { &steady };
        black_box(update_processor.run(settings, &mut update_buffer, CHANNELS));
    });

    let maximum = EqSettings::new(&bands(MAX_BANDS), SAMPLE_RATE, -6.0, true);
    let mut maximum_processor = Processor::new(CHANNELS);
    let mut maximum_buffer = signal(FRAMES * CHANNELS);
    bench("maximum-band", 2_000, || {
        maximum_buffer[0] = 0.25;
        black_box(maximum_processor.run(&maximum, &mut maximum_buffer, CHANNELS));
    });

    let wet = EqSettings::with_bypass(&active_bands, SAMPLE_RATE, -3.0, true, false);
    let dry = EqSettings::with_bypass(&active_bands, SAMPLE_RATE, -3.0, true, true);
    let mut bypass_processor = Processor::new(CHANNELS);
    let mut bypass_buffer = signal(512 * CHANNELS);
    let mut bypassed = false;
    bench("bypass-transition", 50_000, || {
        bypass_buffer[0] = 0.25;
        bypassed = !bypassed;
        let settings = if bypassed { &dry } else { &wet };
        black_box(bypass_processor.run(settings, &mut bypass_buffer, CHANNELS));
    });
}

fn bench(name: &str, iterations: u32, mut run: impl FnMut()) {
    for _ in 0..100 {
        run();
    }
    let start = Instant::now();
    for _ in 0..iterations {
        run();
    }
    let ns = start.elapsed().as_nanos() / u128::from(iterations);
    println!("{name:>20}: {ns:>8} ns/block");
}

fn bands(count: usize) -> Vec<Band> {
    (0..count)
        .map(|index| Band {
            kind: BandKind::Peaking,
            freq: 20.0 * 1_000.0f32.powf(index as f32 / count.max(1) as f32),
            gain_db: if index % 2 == 0 { 3.0 } else { -3.0 },
            q: 1.0,
        })
        .collect()
}

fn signal(samples: usize) -> Vec<f32> {
    (0..samples)
        .map(|index| ((index as f32 * 0.017).sin() * 0.25) + 0.1)
        .collect()
}
