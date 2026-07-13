# Changelog

All notable changes to this project will be documented in this file.

This project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- The on/off state is now persisted (`enabled` in `config.toml`) and restored at daemon
  startup, so an enabled EQ survives a reboot or daemon restart instead of requiring
  `eqtune on` after every login. Restoring respects the Low Power Mode auto-off policy,
  and a failed engine start at startup is logged rather than crash-looping the daemon.
- Unsaved session tuning (band and preamp edits) is mirrored to a `session.toml` draft
  file and restored — still as an unsaved draft — after a daemon restart, so a reboot,
  crash, or reinstall no longer silently discards live edits. The `eqtune off`
  save/overwrite/discard prompt is unchanged. Only preset contents are trusted from the
  draft (preset by preset, for presets the saved config knows); the active preset and
  global toggles always come from the saved config, and an unusable draft is moved
  aside as `session.toml.corrupt`.

### Changed

- `eqtune preset <name>` now persists the switch immediately, like the global toggles.
  Switching presets no longer counts as "unsaved tuning changes", so `eqtune off` right
  after a switch no longer raises the save/overwrite/discard prompt (it still does for
  actual band/preamp edits), and preset-management commands are no longer blocked by a
  mere switch.

### Fixed

- CLI invocations no longer panic with "failed printing to stdout: Broken pipe" when
  their output pipe closes early (e.g. `eqtune status | head`); they now exit quietly
  like any Unix filter. The daemon still ignores `SIGPIPE`, so a client disconnecting
  mid-response cannot kill it.
- The `eqtune off` save prompt's save-by-name path now accepts the active preset's own
  name as an overwrite, instead of dead-ending with "preset already exists" for custom
  presets; the name prompt says so. Names of other custom presets are still rejected.

### Waiting to be implemented

- `eqtune limiter on|off` to toggle the existing limiter setting from the CLI.
- `eqtune curve` / `eqtune preset-show [name]` to print EQ bands without changing state.
- CLI support for low-shelf and high-shelf bands, not just peaking filters.
- A flat/bypass preset or `eqtune flat` command for A/B testing while the engine remains active.
- `eqtune preamp-auto` to estimate a conservative make-up gain from the active preset.
- `eqtune response` to print a frequency-response table using the existing DSP math.
- Shell completion generation for zsh, bash, and fish.

## [0.3.1] - 2026-07-06

### Fixed

- Hardened daemon request reads with a total deadline and 64 KiB request-line cap, including
  the read that contains the terminating newline, so stalled or flooding clients cannot
  freeze the single-threaded accept/poll loop.
- Config loading now rejects malformed TOML, over-cap presets, and non-finite or out-of-range
  preset values before they can reach the realtime engine.
- Unusable config files are moved aside as `config.toml.corrupt` (or `.corrupt.N`) before
  falling back to defaults, preserving the bad file for manual recovery instead of
  crash-looping under launchd.
- Config saves now write a sibling temp file, fsync it, atomically rename it into place,
  and fsync the directory to reduce the chance of a crash or power loss truncating the
  live config.
- Reinstalling with `eqtune install` now restarts an already-loaded LaunchAgent with
  `launchctl kickstart -k`, avoiding the bootout/bootstrap race, and skips copying when
  the current executable is already the installed binary.
- `eqtune status` reports the output device the running engine is actually attached to,
  rather than relabeling it as the current default device during the device-follow poll gap.

### Changed

- The realtime processor reserves capacity for the 64-band preset cap up front, so adopting
  a larger preset does not allocate on the audio thread.
- EQ settings snapshots now carry construction-time generation stamps with private fields,
  making live-update detection robust even if an allocator reuses the same heap address.

## [0.3.0] - 2026-06-25

### Added

- Idle auto-off via `eqtune idle on|off`: when captured system audio stays silent, the
  daemon suspends the Core Audio engine and resumes when the default output device
  reports active I/O again.
- Safer validation for EQ edits: band frequency, band gain, Q, and preamp values must be
  finite and inside practical audio ranges before they are persisted or applied live.
- Preset management commands: `preset-save`, `preset-clone`, `preset-rename`, and
  `preset-rm` for creating and managing user-tuned variants.
- Preset sharing commands: `preset-export` writes a single-preset TOML file and
  `preset-import` reads one back, with an optional name override.
- `preset-export` can omit the file path; it defaults to `<preset>.toml` in the current
  directory and prints the resolved path.
- Live tuning edits are now session drafts: `eqtune off` asks whether to save by name,
  overwrite the active preset name, or discard the changes. Saving by name may create a
  new preset or overwrite a shipped preset name (`bright`, `mellow`, `pro`).
- `eqtune reset <preset>` restores one shipped preset, such as `bright`, after local
  overwrites or deletion.
- `eqtune reset` restores all shipped presets while preserving user-created presets.
- Reset commands warn before replacing modified local shipped presets and can save those
  local versions under custom names first.
- `preset-rm` now accepts multiple preset names and removes them in one persisted update.

## [0.2.0] - 2026-06-12

### Added

- Low Power Mode auto-off controls via `eqtune lowpower on|off`, letting the daemon suspend automatically when macOS enables Low Power Mode.

### Changed

- `eqtune on` now explicitly overrides Low Power Mode when requested, so the EQ can keep running even while battery-saving mode is active.
- The EQ engine is lighter in steady state: filter coefficients are only rebuilt when the EQ changes, 0 dB bands are dropped from the live processing chain, and silence is skipped.
- `eqtune on` and edit commands continue to print the resulting curve, and edits still apply live while persisting to the user config file.
