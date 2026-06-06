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

From crates.io (once published):

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
eqtune on | off | status             # start / stop / inspect
eqtune presets | preset <name>        # list / switch preset (applies live)
eqtune band <freq_hz> <gain_db> [q]   # add or update a band (negative gains OK)
eqtune band-rm <freq_hz>              # remove the band nearest a frequency
eqtune preamp <db>                    # overall make-up gain
eqtune reset                          # restore the shipped presets
eqtune install | uninstall            # manage the launchd daemon
```

- Edits apply **live** (no audio restart) and persist to  `~/Library/Application Support/eqtune/config.toml`.
- For the no-eqtune native Apple sound, use `eqtune off`.

## Presets

| Preset | Character |
|--------|-----------|
| `bright` *(default)* | brighter, more presence |
| `mellow` | warmer |
| `pro` | crisp and detailed |

Switch with `eqtune preset <name>`, then fine-tune live with `eqtune band` / `eqtune preamp`.

## Tweak your own

The EQ is fully editable. `eqtune band` adds or updates a peaking filter at any frequency on the active preset:

```sh
eqtune band 2000 -6        # cut 2 kHz by 6 dB (default Q 1.41)
eqtune band 8000 3 2.0     # boost 8 kHz by 3 dB with a narrower Q
eqtune band-rm 2000        # remove the 2 kHz band
eqtune preamp 4            # set the preamp to +4 dB
```

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

## Uninstall

```sh
make uninstall      # or: eqtune uninstall
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.
