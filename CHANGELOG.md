# Changelog

All notable changes to this project will be documented in this file.

This project follows [Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-06-12

### Added

- Low Power Mode auto-off controls via `eqtune lowpower on|off`, letting the daemon suspend automatically when macOS enables Low Power Mode.

### Changed

- `eqtune on` now explicitly overrides Low Power Mode when requested, so the EQ can keep running even while battery-saving mode is active.
- The EQ engine is lighter in steady state: filter coefficients are only rebuilt when the EQ changes, 0 dB bands are dropped from the live processing chain, and silence is skipped.
- `eqtune on` and edit commands continue to print the resulting curve, and edits still apply live while persisting to the user config file.
