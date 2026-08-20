# eqtune

[![Crates.io](https://img.shields.io/crates/v/eqtune.svg)](https://crates.io/crates/eqtune)
[![docs.rs](https://docs.rs/eqtune/badge.svg)](https://docs.rs/eqtune)
[![CI](https://github.com/y7nieSEl5/eqtune/actions/workflows/ci.yml/badge.svg)](https://github.com/y7nieSEl5/eqtune/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/eqtune.svg)](LICENSE-MIT)

A lightweight, system-wide audio equalizer for macOS.

## Why

Mac speakers and headphone outputs are tuned conservatively out of the box — often mid-heavy and a bit flat, so music can sound closed-in. macOS has **no built-in system-wide EQ**, and the existing tools tend to be heavyweight: they install loopback/kernel drivers and *replace* your default output device, which breaks macOS's normal "switch to the headphones when I plug them in" behaviour.

eqtune taps the system audio mix with Apple's modern **Core Audio process-tap API** (macOS 14.2+, no driver, no kernel extension, no Developer ID certificate), applies a parametric EQ, and plays the result back to your **current** output device. Because it never hijacks the default device, plugging in EarPods or Bluetooth keeps working normally. It ships a few curated presets and lets you tweak any frequency yourself.

## Requirements

- macOS 14.2 or later (the process-tap API)
- Xcode Command Line Tools — `xcode-select --install` (clang + CoreAudio)
- Rust — https://rustup.rs

eqtune currently processes matching interleaved stereo Float32 aggregate streams. If an
output device exposes an unsupported layout, eqtune refuses to start the tap and records
the reason in `~/Library/Application Support/eqtune/daemon.log` instead of risking
corrupted audio.

## Install

From crates.io:

```sh
cargo install --locked eqtune
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

To upgrade, install the new crate and refresh the managed daemon copy:

```sh
cargo install --locked eqtune --force
eqtune install
```

[GitHub Releases](https://github.com/y7nieSEl5/eqtune/releases) contains versioned source
archives and release notes. The supported installable distribution is the crates.io package.

## Usage

```
eqtune on | off | status              # start / stop / inspect
eqtune presets | preset <name>        # list / switch preset (short: ls / p <name>)
eqtune preset-show [name]             # show active or named preset without switching
eqtune preset-save <name>             # save active tuning as a preset
eqtune preset-clone <src> <name>      # clone a preset and switch to the clone
eqtune preset-rename <old> <new>      # rename a preset
eqtune preset-rm <name> [name...]     # delete one or more presets
eqtune preset-export <name> [file]    # write a shareable preset TOML file
eqtune preset-import <file> [name]    # import a preset, optionally renaming it
eqtune band <freq_hz> <gain_db> [q]   # add or update a band (negative gains OK)
eqtune band-rm <freq_hz>              # remove the band at a configured frequency
eqtune preamp <db>                    # overall make-up gain
eqtune limiter on | off               # toggle the soft limiter (default on)
eqtune lowpower on | off              # auto-off in macOS Low Power Mode (default on)
eqtune idle on | off                  # auto-off while no media is active (default on)
eqtune reset [preset]                 # restore all shipped presets, or one preset
eqtune completions bash|zsh|fish      # print a shell completion script
eqtune install | uninstall            # manage the launchd daemon
```

- `eqtune on` and every edit (`preset`/`band`/`band-rm`/`preamp`/`reset`) print the
  resulting curve — the active preset, preamp, and each band — with the band you just
  changed flagged. `eqtune off` confirms the native Apple audio path is restored.
- Tuning edits apply **live** (no audio restart). If you edited bands or the preamp,
  `eqtune off` asks whether to save the latest tuning as a new preset, overwrite the
  active preset name, or discard the session changes. Switching presets is saved
  immediately and never triggers that prompt by itself; edits stay attached to the
  preset you made them on, and the prompt names every preset that still has unsaved
  edits.
- eqtune **remembers its state across restarts**: after a reboot (or daemon restart) it
  comes back on if you left it on, with the preset — including tuning edits you haven't
  saved yet — you were listening to. Unsaved edits stay unsaved; the `eqtune off` prompt
  still decides whether to keep them.
- For the no-eqtune native Apple sound, use `eqtune off`.
- To save battery, eqtune **auto-disables while no media is active** and resumes when
  playback starts again. It also auto-disables while macOS Low Power Mode is on and
  resumes when it turns off. An explicit `eqtune on` overrides Low Power Mode; turn
  these behaviours off with `eqtune idle off` or `eqtune lowpower off`.

### Shell completion

After installing eqtune, add the block for your shell to its startup file. These forms
generate completions from the installed binary at shell startup, so they automatically
pick up new commands after an eqtune upgrade.

```sh
# zsh (~/.zshrc)
autoload -Uz compinit
compinit
source <(eqtune completions zsh)

# bash (~/.bashrc)
source <(eqtune completions bash)

# fish (~/.config/fish/config.fish)
eqtune completions fish | source
```

Zsh's `source` line must run after `compinit`, which defines the `compdef` function used
by the generated script. Frameworks such as Oh My Zsh commonly initialize `compinit`
themselves; in that case, omit the two initialization lines above and put only the
`source <(...)` line after the framework setup. Reload the startup file (for example,
`source ~/.zshrc`) or open a new terminal, then type `eqtune ` and press Tab.

Bash needs no separate initialization or `bash-completion` package for this generated
script; it uses Bash's built-in programmable-completion commands. Fish completion is
enabled natively, so sourcing the generated Fish script is also sufficient. In every
case, `eqtune` must already be installed and available on `PATH` when the startup file
runs.

## Presets

| Preset | Character |
|--------|-----------|
| `bright` *(default)* | brighter, more presence |
| `mellow` | warmer |
| `pro` | crisp and detailed |

Inspect the active curve with `eqtune preset-show`, or any preset without switching to it
with `eqtune preset-show <name>`. Switch with `eqtune preset <name>` (or just `eqtune p
<name>`), then fine-tune live with `eqtune band` / `eqtune preamp`.
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
The soft limiter is enabled by default to keep boosted output bounded; advanced users can
toggle it globally with `eqtune limiter on|off`. The change is persisted immediately and
applies live without becoming part of a preset's unsaved tuning session.

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
