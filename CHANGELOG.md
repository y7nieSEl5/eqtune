# Changelog

All notable changes to this project will be documented in this file.

This project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Planned

- CLI support for low-shelf and high-shelf bands, not just peaking filters.
- A flat/bypass preset or `eqtune flat` command for A/B testing while the engine remains active.
- `eqtune preamp-auto` to estimate a conservative make-up gain from the active preset.
- `eqtune response` to print a frequency-response table using the existing DSP math.
- Per-output-device preset assignment for automatically selecting speaker, headphone, or
  DAC tuning.
- Machine-readable response export (CSV/JSON), dry/wet control, and click-free active
  bypass for measurement and A/B workflows.
- Limiter threshold/headroom controls, preset metadata and schema versioning, and
  non-interactive save/reset options for scripts.
- Structured engine diagnostics including capture permission, stream format, channel
  layout, suspension reason, and the last engine error.
- Offline DSP benchmarks and callback timing telemetry, followed by event-driven Core
  Audio listeners to replace fixed-rate device and power-state polling where practical.

## [0.5.0] - 2026-08-10

### Added

- `eqtune preset-show [name]` prints the active or named preset's preamp and bands
  without switching presets or changing configuration.
- `eqtune limiter on|off` now toggles the global soft limiter from the CLI, persists the
  setting immediately, and applies it live without creating an unsaved tuning session.
- `eqtune completions bash|zsh|fish` generates shell completion scripts directly from
  the CLI definition without contacting the daemon.

### Changed

- The obsolete `auto_follow_new_devices` config field has been removed. Output-device
  following remains always on; older configs containing the unused key continue to load,
  and the key disappears on the next config save.
- The build now renders the embedded Info.plist from a template using the Cargo package
  version, so macOS metadata cannot drift behind the released binary version.

### Fixed

- Sustained-silence processing now clears biquad delay memory before skipping DSP work,
  preventing an old low-frequency or high-Q filter tail from reappearing when playback
  resumes.
- Live band insertions, removals, and edits no longer let a band inherit filter state from
  a different band that previously occupied the same cascade index.
- The Core Audio shim validates matching interleaved stereo Float32 input/output formats
  before starting and safely rejects an unexpected runtime buffer topology instead of
  reinterpreting arbitrary device buffers as floats.
- Config loading now rejects and preserves configs with an empty preset library or a
  missing active preset instead of accepting a state that normal tuning commands cannot use.
- CLI control connections now use read/write deadlines, cap daemon replies at 1 MiB, and
  report an empty reply, preventing a stuck or invalid daemon from hanging or growing the
  client without bound.

## [0.4.0] - 2026-07-21

### Added

- The on/off state is now persisted (`enabled` in `config.toml`) and restored at daemon
  startup, so an enabled EQ survives a reboot or daemon restart instead of requiring
  `eqtune on` after every login. Restoring respects the Low Power Mode auto-off policy —
  and, when idle auto-off is enabled, restores *suspended* and starts on the first
  playback rather than running the tap through startup silence — and a failed engine
  start at startup is logged rather than crash-looping the daemon.
  `eqtune on` records the state only after the engine actually started (a failed start
  is never restored later as a silent "on"), and `eqtune off` always stops the engine
  first — a failed config write is reported and retryable, but never keeps audio
  processing (nor lets a later Low Power Mode cycle restore an EQ you turned off).
- Unsaved session tuning (band and preamp edits) is mirrored to a `session.toml` draft
  file and restored — still as an unsaved draft — after a daemon restart, so a reboot,
  crash, or reinstall no longer silently discards live edits. The `eqtune off`
  save/overwrite/discard prompt is unchanged. Only preset contents are trusted from the
  draft (preset by preset, for presets the saved config knows); the active preset and
  global toggles always come from the saved config, and an unusable draft is moved
  aside as `session.toml.corrupt`. Draft writes are rate-limited, so a burst of edits
  (dragging a control) coalesces into a few writes instead of rewriting the whole config
  on every step.

### Changed

- `eqtune preset <name>` now persists the switch immediately, like the global toggles.
  Switching presets no longer counts as "unsaved tuning changes", so `eqtune off` right
  after a switch no longer raises the save/overwrite/discard prompt (it still does for
  actual band/preamp edits), and preset-management commands are no longer blocked by a
  mere switch.

### Fixed

- `eqtune install` now stages daemon binary updates as a sibling temp file, ad-hoc signs
  the staged copy before atomically replacing the installed daemon, and verifies launchd
  reaches the running state. If an already-loaded service keeps stale launch constraints
  and `kickstart -k` leaves it spawn-failed, install falls back to bootout + bootstrap
  instead of reporting success over a dead daemon.
- CLI invocations no longer panic with "failed printing to stdout: Broken pipe" when
  their output pipe closes early (e.g. `eqtune status | head`); they now exit quietly
  like any Unix filter. The daemon still ignores `SIGPIPE`, so a client disconnecting
  mid-response cannot kill it.
- The `eqtune off` save prompt's save-by-name path now accepts the active preset's own
  name as an overwrite, instead of dead-ending with "preset already exists" for custom
  presets; the name prompt says so. Names of other custom presets are still rejected.
- Saving the session by name no longer silently reverts unsaved edits left on a
  previously active preset (edit `bright`, switch to `mellow`, `off`, save — the
  `bright` edits used to be dropped while the CLI printed "saved tuning"). Those edits
  now stay an open session that the next `eqtune off` asks about, and the prompt names
  every preset that actually carries unsaved edits instead of showing only the active
  curve.
- `eqtune preset-clone` is now rejected while unsaved tuning changes are active, like
  the other preset-management commands — it used to rebuild the working config from the
  saved one and silently drop the session edits.
- `eqtune lowpower off` no longer restarts the engine while it is idle-suspended with no
  media playing; the idle policy keeps it suspended until playback resumes.
- Preset-management commands (`preset-rm`, `preset-rename`, `preset-import`, `reset`) and
  the save prompt's overwrite path no longer adopt a change in memory when the config
  write fails. A failed save now leaves `status`, the engine, and the disk in agreement
  and retrying the command re-attempts the write — it used to leave a half-applied change
  behind that read as phantom unsaved tuning. A failed `reset` also no longer lifts an
  idle suspension.

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

[Unreleased]: https://github.com/y7nieSEl5/eqtune/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/y7nieSEl5/eqtune/releases/tag/v0.5.0
