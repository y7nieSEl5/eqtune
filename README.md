# eqtune

A lightweight, system-wide audio equalizer for macOS.

## Why

Mac speakers and headphone outputs are tuned conservatively out of the box — often mid-heavy and a bit flat, so music can sound closed-in. macOS has **no built-in system-wide EQ**, and the existing tools tend to be heavyweight: they install loopback/kernel drivers and *replace* your default output device, which breaks macOS's normal "switch to the headphones when I plug them in" behaviour.

eqtune taps the system audio mix with Apple's modern **Core Audio process-tap API** (macOS 14.2+, no driver, no kernel extension, no code signing), applies a parametric EQ, and plays the result back to your **current** output device. Because it never hijacks the default device, plugging in EarPods or Bluetooth keeps working normally. It ships a few curated presets and lets you tweak any frequency yourself.

## Requirements

- macOS 14.2 or later (the process-tap API)
- Xcode Command Line Tools — `xcode-select --install` (clang + CoreAudio)
- Rust — https://rustup.rs

## Install

From crates.io:

```sh
cargo install eqtune
eqtune install
eqtune on
```

Or from a clone:

```sh
cd eqtune
make install             
eqtune on
```

On the first `eqtune on`, macOS asks for audio-capture permission.
(Rebuilding changes the binary's ad-hoc signature, so macOS may re-ask. That's expected.)

## Usage

```
eqtune on | off | status              # start / stop / inspect
eqtune presets | preset <name>        # list / switch preset (short: ls / p <name>)
eqtune preset-save <name>             # save active tuning as a preset
eqtune preset-clone <src> <name>      # clone a preset and switch to the clone
eqtune preset-rename <old> <new>      # rename a preset
eqtune preset-rm <name> [name...]     # delete one or more presets
eqtune preset-export <name> [file]    # write a shareable preset TOML file
eqtune preset-import <file> [name]    # import a preset, optionally renaming it
eqtune band <freq_hz> <gain_db> [q]   # add or update a band (negative gains OK)
eqtune band-rm <freq_hz>              # remove the band nearest a frequency
eqtune preamp <db>                    # overall make-up gain
eqtune lowpower on | off              # auto-off in macOS Low Power Mode (default on)
eqtune idle on | off                  # auto-off while no media is active (default on)
eqtune reset [preset]                 # restore all shipped presets, or one preset
eqtune install | uninstall            # manage the launchd daemon
```

- `eqtune on` and every edit (`preset`/`band`/`band-rm`/`preamp`/`reset`) print the
  resulting curve — the active preset, preamp, and each band — with the band you just
  changed flagged. `eqtune off` confirms the native Apple audio path is restored.
- Tuning edits apply **live** (no audio restart). On `eqtune off`, eqtune asks whether
  to save the latest tuning as a new preset, overwrite the active preset name, or discard
  the session changes.
- For the no-eqtune native Apple sound, use `eqtune off`.
- To save battery, eqtune **auto-disables while no media is active** and resumes when
  playback starts again. It also auto-disables while macOS Low Power Mode is on and
  resumes when it turns off. An explicit `eqtune on` overrides Low Power Mode; turn
  these behaviours off with `eqtune idle off` or `eqtune lowpower off`.

## Presets

| Preset | Character |
|--------|-----------|
| `bright` *(default)* | brighter, more presence |
| `mellow` | warmer |
| `pro` | crisp and detailed |

Switch with `eqtune preset <name>` (or just `eqtune p <name>`), then fine-tune live with `eqtune band` / `eqtune preamp`.
You can temporarily tune any preset, including `bright`, `mellow`, and `pro`; when you
turn eqtune off, choose whether to save the result as a new preset, overwrite that preset
name on your device, or discard it. When saving by name, entering `bright`, `mellow`, or
`pro` overwrites that shipped preset on your device; entering any unused name creates your
own preset. If you later regret overwriting a shipped preset, run
`eqtune reset bright` (or `mellow` / `pro`) to restore the original shipped tuning;
`eqtune reset` restores all three shipped presets while keeping your custom presets. If a
reset would replace a modified local shipped preset, eqtune warns first and can save that
local version under a new custom preset name before restoring the original.
New custom preset names use ASCII letters, digits, `-`, `_`, or `.`, and never overwrite
an existing custom preset.
Share presets with `eqtune preset-export my-bright`, which writes `my-bright.toml` in the
current directory, or pass an explicit path. Import them with `eqtune preset-import
my-bright.toml` or `eqtune preset-import my-bright.toml other-name`.

## Tweak your own

The EQ is fully editable. `eqtune band` adds or updates a peaking filter at any frequency on the active preset:

```sh
eqtune band 2000 -6        # cut 2 kHz by 6 dB (default Q 1.0)
eqtune band 8000 3 2.0     # boost 8 kHz by 3 dB with a narrower Q
eqtune band-rm 2000        # remove the 2 kHz band
eqtune preamp 4            # set the preamp to +4 dB
```

Editable values are validated before they reach the audio engine: band frequencies must
be 20-20000 Hz, band gains -24 to +24 dB, Q 0.1-10, and preamp -60 to +12 dB.
Presets are capped at 64 bands. If a hand-edited config is malformed or contains values
the realtime engine cannot run, eqtune preserves it as `config.toml.corrupt` (or a
numbered sibling) and starts from shipped defaults instead of crash-looping.

## Battery & energy

eqtune is an always-on background daemon that **taps all system audio and re-processes
every block in real time**. That continuous work costs CPU, and on battery it adds up
fast — stream music for a couple of hours and you'll see the charge drop noticeably
quicker than with Apple's native audio path.

> **FYI:** running a system-wide EQ **increases battery drain, sometimes dramatically.**
> A continuously-running real-time audio pipeline simply uses more power than native
> playback. If battery life matters to you, leave the Low Power Mode auto-off enabled
> (the default) and run `eqtune off` when you don't need the EQ.

**Idle auto-off.** When captured system audio stays silent, eqtune tears the audio engine
down automatically and brings it back when the default output device reports active I/O
again. Disable this behaviour with `eqtune idle off`.

**Low Power Mode auto-off.** When macOS switches on Low Power Mode, eqtune tears the
audio engine down automatically (the single biggest saving) and brings it back when Low
Power Mode turns off. An explicit `eqtune on` still overrides and runs even under Low
Power Mode; disable the behaviour entirely with `eqtune lowpower off`.

**Lighter real-time processing.** Recent versions cut the per-block cost of the EQ so a
long listening session draws less power:

- **No redundant rebuilds** — filter coefficients are recomputed only when you actually
  change the EQ, not on every audio block.
- **No-op bands dropped** — bands sitting at 0 dB are mathematical "do nothing" filters;
  they're removed from the live processing chain (the `pro` preset alone sheds ~5 of 28).
- **Idle suspension and silence skipping** — when nothing is playing, eqtune can suspend
  the engine entirely; before suspension, sustained silence also skips per-sample work.

These trim the overhead but can't remove it — system-wide real-time audio always costs
some power while the engine is active. See [ARCHITECTURE.md](ARCHITECTURE.md) for how the
engine and signal path work.

## How it works

```
system audio ─▶ global process tap (excludes eqtune; muted-when-tapped)
             ─▶ private aggregate device (output device + tap, one shared clock)
             ─▶ IOProc: capture → biquad EQ + preamp + soft limiter → replay
             ─▶ your current default output device
```

A launchd LaunchAgent runs the daemon; a Unix-socket CLI controls it. Putting the tap
and the output device in a single aggregate device means they share one clock, so
there's no resampling/drift to fight. A lightweight poll makes the engine follow
default-device changes (plug in headphones and audio follows).

For a deeper dive — the daemon/CLI split, the lock-free real-time DSP, the Objective-C
Core Audio shim, and *why* it's built this way — see [ARCHITECTURE.md](ARCHITECTURE.md).

## Uninstall

```sh
make uninstall      # or: eqtune uninstall
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.
