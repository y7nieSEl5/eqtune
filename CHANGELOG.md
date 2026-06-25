# Changelog

All notable changes to this project will be documented in this file.

This project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Idle auto-off via `eqtune idle on|off`: when captured system audio stays silent, the
  daemon suspends the Core Audio engine and resumes when the default output device
  reports active I/O again.
- Safer validation for EQ edits: band frequency, band gain, Q, and preamp values must be
  finite and inside practical audio ranges before they are persisted or applied live.

### Waiting to be implemented

- `eqtune limiter on|off` to toggle the existing limiter setting from the CLI.
- Preset clone/save/delete/rename commands for managing user-tuned variants.
- `eqtune curve` / `eqtune preset-show [name]` to print EQ bands without changing state.
- CLI support for low-shelf and high-shelf bands, not just peaking filters.
- A flat/bypass preset or `eqtune flat` command for A/B testing while the engine remains active.
- Preset import/export for sharing or backing up individual tunings.
- `eqtune preamp-auto` to estimate a conservative make-up gain from the active preset.
- `eqtune response` to print a frequency-response table using the existing DSP math.
- Shell completion generation for zsh, bash, and fish.

## [0.2.0] - 2026-06-12

### Added

- Low Power Mode auto-off controls via `eqtune lowpower on|off`, letting the daemon suspend automatically when macOS enables Low Power Mode.

### Changed

- `eqtune on` now explicitly overrides Low Power Mode when requested, so the EQ can keep running even while battery-saving mode is active.
- The EQ engine is lighter in steady state: filter coefficients are only rebuilt when the EQ changes, 0 dB bands are dropped from the live processing chain, and silence is skipped.
- `eqtune on` and edit commands continue to print the resulting curve, and edits still apply live while persisting to the user config file.
