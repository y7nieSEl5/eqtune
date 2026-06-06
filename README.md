# eqtune

A lightweight, system-wide audio equalizer for macOS — a single Rust binary you
build and run yourself.

## What it does

macOS has no public API to insert an EQ into the global output mix, so eqtune captures
all system audio via the **Core Audio process-tap API** (macOS 14.2+), runs it through
a parametric biquad EQ + preamp, and replays it to whatever your **current default
output device** is. Because it never replaces the default output device, plugging in
EarPods/Bluetooth keeps working normally — the engine just follows.

## Requirements

- macOS 14.2 or later (the process-tap API)
- Xcode Command Line Tools — `xcode-select --install` (clang + CoreAudio frameworks)
- Rust — https://rustup.rs

No Apple Developer account, code signing, or notarization needed: you build from source,
and locally built code is never quarantined, so Gatekeeper never gets involved.

## Install

```sh
git clone <repo> && cd eqtune
make install      # builds --release + loads the LaunchAgent daemon
eqtune on         # after granting the audio-capture permission prompt
```

`make install` prints an optional one-liner to symlink the `eqtune` CLI onto your PATH.
The first time the daemon taps audio, macOS asks for audio-capture permission — grant
it. (Rebuilding changes the binary's ad-hoc signature, so macOS may re-ask; expected.)

## Usage

```
eqtune on | off | status
eqtune presets | preset <name>
eqtune band <freq_hz> <gain_db> [q]   # add/update a band (negative gains OK)
eqtune band-rm <freq_hz>
eqtune preamp <db>
eqtune reset
eqtune install | uninstall
```

Edits apply live (no audio restart) and persist to
`~/Library/Application Support/eqtune/config.toml`.

## Default tuning

The built-in `default` preset (graphic-EQ-style, peaking filters at Q≈1.41):

| 32 | 64 | 125 | 500 | 1k  | 2k  | 4k | 8k | 16k | preamp |
|----|----|-----|-----|-----|-----|----|----|-----|--------|
| -5 | -5 | -5  | -5  | -10 | -15 | -4 | +2 | 0   | +7 dB  |

There's also a `flat` preset. Adjust live with `eqtune band` / `eqtune preamp`, or edit
the config file.

## How it works

```
system audio ─▶ global process tap (excludes eqtune; muted-when-tapped)
             ─▶ private aggregate device (output device + tap, one shared clock)
             ─▶ IOProc: capture → biquad EQ + preamp + soft limiter → replay
             ─▶ your default output device
```

A launchd LaunchAgent runs the daemon; a Unix-socket CLI controls it. Putting the tap
and the output device in a single aggregate device means they share one clock, so there
is no resampling/drift compensation to fight.

## Uninstall

```sh
make uninstall    # or: eqtune uninstall
```

## License

TBD.
