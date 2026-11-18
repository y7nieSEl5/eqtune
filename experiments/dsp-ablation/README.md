# DSP component ablation — 2026-09-04

This small offline experiment measures how EQ bands, preamp headroom, the limiter, and
dry bypass affect DSP processing cost and output. It is preserved as a reference for
anyone who wants to reuse the harness or build on the measurements. It makes no changes
to production DSP or live audio settings.

The recorded run used the unmodified production DSP at commit `3e9bc46` (eqtune 0.7.1).
Its working-tree lockfile already contained dependency updates; a copy is preserved as
[Cargo.lock](Cargo.lock) alongside the results, without changing the repository's active
dependency versions. Host: aarch64-apple-darwin, macOS 26.6.2 (25G83), rustc 1.95.0,
optimized Cargo bench profile. CPU model was unavailable through the sandbox.

## Findings

- Removing all EQ bands reduced normal-input median time by 81–93% across the three presets. This changes the sound; it is a cost attribution experiment, not an optimization recommendation.
- Removing the limiter reduced normal-input median time by only 0.5–1.1%, with identical output on this input. These small timing differences are comparable to measurement variability.
- On loud bright input, removing the limiter reduced median time from 7.081 to 6.040 µs/block (14.7%), but increased output peak from 0.981 to 1.314. 287 of 8192 measured channel samples (3.50%) exceeded full scale without limiting.
- Removing bright’s -8 dB preamp attenuation increased normal-input peak from 0.328 to 0.825. On loud input it increased median processing time from 7.081 to 10.074 µs/block as more samples required nonlinear limiting.
- Dry bypass retained roughly the full processing cost because it continues advancing the wet filters. Its output matched the original input exactly in every tested workload.
- Sustained exact silence took about 0.35 µs/block for every variant. This observes the existing silence fast path; it does not independently ablate that optimization.

## Method

48 kHz, stereo, 256-frame blocks (5.333 ms of audio). Three shipped presets: bright (10 active bands, -8 dB preamp), mellow (10 active bands, 0 dB), and pro (23 active bands, 0 dB). The full configuration enables the limiter. Each ablation changes just one setting; dry bypass is an additional comparison.

Inputs are deterministic periodic stereo multitones at approximately 58.6, 503.9, 2003.9, and 8003.9 Hz, with different channel phases. Normal input has a measured peak of 0.241774; loud input peaks at 0.967095. Silence is exact zero. Every call receives fresh original input, never the previous processed output.

Each timing sample uses a new processor, 256 warmup blocks, then 4096 timed blocks. Seven rounds rotate and reverse variant order. Settings construction and warmup are excluded; identical input-copy and black-box overhead is included. Output metrics use a separate warmed processor over one 4096-frame period (8192 channel samples). The same deterministic output metrics repeat on each round’s CSV row.

`unity_preamp` sets gain to 0 dB; it does not remove the multiplication instruction. It is an identical-output control for mellow and pro, which already use 0 dB. `no_eq` removes all bands but retains the preamp and limiter. `no_limiter` retains bands and preamp. `dry_bypass` selects the dry endpoint while keeping the wet state warm.

## Timing

Median µs/block across seven rounds:

| Preset | Input | Full | No EQ | Unity preamp | No limiter | Dry bypass |
|---|---|---:|---:|---:|---:|---:|
| bright | normal | 6.072 | 1.110 | 6.055 | 6.023 | 6.099 |
| bright | loud | 7.081 | 1.115 | 10.074 | 6.040 | 7.101 |
| bright | silence | 0.347 | 0.350 | 0.349 | 0.348 | 0.349 |
| mellow | normal | 6.093 | 1.139 | 6.118 | 6.025 | 6.118 |
| mellow | loud | 6.085 | 1.147 | 6.080 | 6.056 | 6.106 |
| mellow | silence | 0.347 | 0.348 | 0.349 | 0.348 | 0.350 |
| pro | normal | 15.865 | 1.132 | 15.839 | 15.785 | 16.037 |
| pro | loud | 15.934 | 1.286 | 15.908 | 15.810 | 16.046 |
| pro | silence | 0.353 | 0.355 | 0.364 | 0.348 | 0.348 |

The host was not isolated from other work. In particular, pro/loud rounds had timing outliers: full ranged from 15.831 to 23.425 µs/block; no-limiter ranged from 15.706 to 39.215. Medians summarize throughput; these are not callback deadline or tail-latency guarantees. Small differences should not be interpreted as established speedups.

## Loud-input output metrics

| Preset | Variant | Peak amplitude | RMS difference from full | Samples above unity / 8192 |
|---|---|---:|---:|---:|
| bright | full | 0.980544 | 0.000000 | 0 |
| bright | no_eq | 0.385008 | 0.351990 | 0 |
| bright | unity_preamp | 0.996001 | 0.350756 | 0 |
| bright | no_limiter | 1.313985 | 0.029959 | 287 |
| bright | dry_bypass | 0.967095 | 0.216567 | 0 |
| mellow | full | 0.887320 | 0.000000 | 0 |
| mellow | no_eq | 0.940154 | 0.167591 | 0 |
| mellow | unity_preamp | 0.887320 | 0.000000 | 0 |
| mellow | no_limiter | 0.887320 | 0.000000 | 0 |
| mellow | dry_bypass | 0.967095 | 0.167623 | 0 |
| pro | full | 0.922369 | 0.000000 | 0 |
| pro | no_eq | 0.940154 | 0.283255 | 0 |
| pro | unity_preamp | 0.922369 | 0.000000 | 0 |
| pro | no_limiter | 0.928814 | 0.000135 | 0 |
| pro | dry_bypass | 0.967095 | 0.283308 | 0 |

RMS difference quantifies changed samples, not perceptual quality. No listening study, real music corpus, Core Audio routing, daemon overhead, settings transitions, energy use, or silence-skip/identity-elimination/generation-cache implementation ablations were measured.

## Reproduce and validation

From the repository root on macOS:

```sh
cargo bench --locked --bench dsp_ablation > /tmp/eqtune-dsp-ablation.csv
cargo test --locked --lib dsp::tests
```

Add `--offline` when the locked dependencies are already cached. These commands use the
current checkout's DSP, presets, and root lockfile. Keep new CSV output separate from
the checked-in historical results; timings vary with the machine, compiler, and load.
For the recorded source/dependency baseline, use a separate checkout of `3e9bc46`, copy
in `benches/dsp_ablation.rs` and its `[[bench]]` entry from the current `Cargo.toml`, and
copy this directory's saved `Cargo.lock` to that checkout's root before running.

The benchmark completed all 315 timing samples and asserted finite output, bounded output with limiting, zero output for sustained silence, and exact dry-bypass equality. All 18 existing DSP tests passed. Formatting and diff whitespace checks passed. Production DSP and live audio settings were unchanged.

## Build on it

The [runner](../../benches/dsp_ablation.rs) keeps the presets, settings variants, input
generator, and timing constants in one file. Add a preset or variant in `main`, change
the sample rate or block size at the top, or supply other input signals. Restore fresh
input before every processed block and keep warmup outside timing. When changing the
input period, update `BLOCKS` and the frequency bins together to keep it continuous.

Record the source commit, dependency lockfile, host, compiler, settings, and raw CSV for
new comparisons. Each CSV row contains preset, input, variant, round, nanoseconds per
block, peak amplitude, RMS amplitude, RMS difference from full processing, and the count
of samples above unity. Compare timing medians across rounds and inspect output changes
alongside speed. Preserve the finite-output, limiter-bound, silence, and dry-bypass
checks for variants that retain those behaviors. Battery use and live routing need
separate measurements.

Raw measurements: [results.csv](results.csv). Benchmark: [dsp_ablation.rs](../../benches/dsp_ablation.rs).
