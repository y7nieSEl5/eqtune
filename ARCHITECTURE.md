# eqtune architecture

eqtune is a system-wide audio equalizer for macOS. It works by **tapping the whole
system audio mix, running a parametric EQ over it in real time, and replaying the result
to your current output device** — without installing a driver, a kernel extension, or
hijacking the default device.

This document explains how the pieces fit together, how each part is realized in code,
and the three design questions behind the project: why it's a standalone CLI/daemon
rather than something built into macOS, why it's written in Rust, and why a slice of it
*has* to be Objective-C.

---

## 1. The shape of the system

eqtune is one binary that plays three roles, split across two communicating processes
plus a thin native shim:

```
   you ── eqtune on/off/band/… ─▶  ┌─────────────────────┐
   (CLI client, short-lived)        │  thin client        │
                                     └──────────┬──────────┘
                                                │  one JSON request / reply
                                                │  over a Unix domain socket
                                                ▼
   launchd ── runs at login ──▶     ┌─────────────────────┐
   (KeepAlive)                       │  daemon (long-lived)│  owns config + audio engine
                                     └──────────┬──────────┘
                                                │  Rust → C FFI (tap_shim.h)
                                                ▼
                                     ┌─────────────────────┐
                                     │  Objective-C shim   │  Core Audio / Foundation
                                     └──────────┬──────────┘
                                                │  process-tap API
                                                ▼
   system audio ─▶ device-scoped tap ─▶ aggregate device ─▶ IOProc ─▶ default output
                                         (capture → EQ → replay, one shared clock)
```

The same executable becomes the **client** or the **daemon** depending on the
subcommand: `eqtune daemon` (hidden, launched by launchd) runs the long-lived process;
control commands (`on`, `off`, `band`, `preset`, …) are thin clients that open the
socket, send **one** request, print the reply, and exit. `completions` is local-only: it
generates a bash, zsh, or fish completion script from the clap command definition and
never contacts the daemon.

### Two planes

It helps to think of eqtune as two independent planes:

- **Control plane** — how you talk to it. A Unix-domain socket carrying newline-delimited
  JSON. Low frequency, human-driven, never touches the audio thread directly.
- **Audio plane** — the real-time loop. A Core Audio I/O callback that fires hundreds of
  times a second and must never block. The control plane hands it new settings
  *lock-free* so a live EQ edit never waits on a control-thread lock.

Keeping these planes decoupled is the central design idea. Filter, preamp, and limiter
edits are adopted at an audio-block boundary without smoothing. Runtime bypass is the one
exception: it ramps between dry and wet over 10 ms.

---

## 2. Module map

| File | Responsibility |
|------|----------------|
| `src/main.rs` | CLI parsing (clap). Dispatches to the daemon or sends one client request. |
| `src/ipc.rs` | The control protocol: `Request`/`Response`/`Status` enums, socket path, send/recv. |
| `src/daemon.rs` | The long-lived process. Owns config + the audio engine, serves the socket, and runs the engine-lifecycle state machine. |
| `src/dsp.rs` | Pure-Rust signal processing: RBJ biquad design, the preamp, the soft limiter, and the real-time `Processor`. |
| `src/sys.rs` | The Rust↔Objective-C FFI boundary and safe wrappers (`TapSession`, `EqHandle`). |
| `src/config.rs` | Persistent TOML config: presets (bands + preamp) and global toggles. |
| `src/launchd.rs` | Installs/removes the LaunchAgent so the daemon runs at login. |
| `shim/tap_shim.{h,m}` | The Objective-C Core Audio shim, exposed to Rust as a tiny C ABI. |
| `build.rs` | Compiles the shim, links the Apple frameworks, embeds `Info.plist`. |

The dependency direction is clean: everything in `src/` is portable, unit-testable Rust,
and **all** the macOS-specific, unsafe, can't-fail-gracefully code is concentrated behind
`src/sys.rs` and the shim.

---

## 3. Control plane — how commands reach the engine

`src/ipc.rs` defines the entire protocol as two Rust enums:

```rust
enum Request  { Status, Enable, Disable, ListPresets, ShowPreset(Option<String>),
                SetPreset(String),
                SavePreset { name }, ClonePreset { source, dest },
                DeletePresets { names }, RenamePreset { from, to },
                ExportPreset { name, path }, ImportPreset { path, name },
                SetBand { kind, freq, gain_db, q }, RemoveBand { freq },
                SetPreamp(f32), SetPreampAuto, GetResponse, SetBypass(bool),
                SetLimiter(bool),
                SetAutoOffLowPower(bool), SetAutoOffIdle(bool),
                SaveSessionAs { name }, SaveSessionOverwrite, DiscardSession,
                ResetPreset { name }, ConfirmResetPreset { name, backups },
                Reset, ConfirmReset { backups } }
enum Response { Ok, Status(Status), Tuning(Tuning), FrequencyResponse(…),
                BandRemoved { tuning, removed }, Presets { … },
                ResetWouldOverwrite { names },
                UnsavedSession { tuning, dirty_presets }, Error(String) }
```

A client (`eqtune band 2000 -6`) serializes one `Request` to JSON, writes a single line
to `~/Library/Application Support/eqtune/eqtune.sock`, and reads one `Response` line back.
The daemon's accept loop (`Daemon::run`) handles each connection, deserializes the
request, mutates state, and replies. `Enable` and the EQ edits reply with `Tuning` (the
active preset, preamp, and bands) so the CLI can print the resulting curve; `RemoveBand`
removes exactly one band only when its configured frequency matches the request within the
small edit-matching tolerance. Otherwise it leaves the tuning untouched and reports the
nearest configured frequency. A successful response also returns the band it actually
removed so the CLI can name it truthfully. `Disable`
returns `UnsavedSession` instead of `Ok` when live tuning edits have not been resolved;
the response carries the active tuning plus every preset name that still has edits. The
CLI then asks whether to save the active tuning by name, overwrite all dirty presets, or
discard them. Preset save/clone/rename/import reply with `Tuning` because they switch to
the resulting preset. Preset deletion replies with the updated preset list. Multi-delete
requests are prevalidated before mutation, so missing/duplicate names or attempts to
delete every preset leave the config unchanged. Reset requests return
`ResetWouldOverwrite` when modified shipped presets would be replaced; the CLI can then
send a confirm request with optional backup preset names. Export writes a single-preset
TOML file and replies `Ok`. `SetLimiter`, `SetAutoOffLowPower`, and `SetAutoOffIdle`
reply `Ok` and the client renders the confirmation. The limiter toggle is persisted as a
global setting and pushed to a running engine immediately; it is not a preset edit.
`GetResponse` returns compact 1/3-octave points calculated from the same coefficient code
as realtime processing, using the validated running output rate or one exact output
snapshot while stopped. JSON/CSV rendering and file output stay in the short-lived client.
`SetPreampAuto` samples that shared response more densely and applies enough negative
preamp to offset its peak boost. `SetBypass` changes only runtime state.

Because the wire format is "one JSON line in, one JSON line out," the protocol is trivial
to extend (add an enum variant) and trivial to test (`serde_json` round-trip tests live in
`ipc.rs`). There's no long-lived connection, no streaming, no versioning headache — the
client is stateless and the daemon is the single source of truth.

The request read is deliberately bounded: one line must arrive within the socket's total
deadline (currently 5 seconds) and fit under 64 KiB. The daemon checks those bounds after
every buffer append, including the append that contains the terminating newline, so a
silent client, slow byte-drip, or newline-less flood cannot wedge the single-threaded
accept/poll loop.

Daemon startup takes a nonblocking advisory lock before touching the control socket. A
second daemon therefore exits without replacing the first one's socket or competing for
config and Core Audio state. Startup also probes an occupied socket for compatibility with
older daemons; it removes the path only when it is a verified stale Unix socket.

`Status` is intentionally a small, flat control-plane snapshot rather than callback
telemetry. It separates user intent from the actual engine state and includes the current
suspension reason, output UID/name/rate/stream facts, last engine error, bounded retry
state, bypass state, and dirty preset names. Output facts describe the validated running
target, or the last complete attempted target while startup is recovering or exhausted.

**Live edits.** Tuning commands (`SetBand`, `SetPreamp`, `SetPreset`, …) mutate the
daemon's working config and, if the engine is running, push freshly-designed coefficients
to the audio thread via `EqHandle::store` — without restarting playback. Editing commands
(`band`, `band-rm`, `preamp`) are session drafts: the daemon keeps a separate snapshot of
the last saved config, and on `eqtune off` any content difference becomes an interactive
save/overwrite/discard prompt. While a difference exists it is also mirrored to a
session-draft file, so an unresolved session survives a daemon restart (§8). A preset
*switch* is a selection, not an edit: it commits to the saved config immediately (like the
global toggles) and never raises the prompt by itself. Explicit library-management
commands (`preset-save`, import, rename, delete, reset, …) also persist immediately.

---

## 4. Audio plane — the capture → EQ → replay loop

This is the part that needs Apple's frameworks, and it lives in `shim/tap_shim.m`. The
modern **Core Audio process-tap API** (macOS 14.2+) lets a normal user-space process
observe the system audio mix with no driver. eqtune sets up three objects:

1. **A device-scoped process tap** (`AudioHardwareCreateProcessTap` with a
   `CATapDescription`). It captures every process destined for the exact snapshotted
   output UID and stream while **excluding eqtune's own process** — otherwise replay would
   feed back into capture. Core Audio makes the tap match that selected stream's format,
   so a 44.1 kHz output produces a 44.1 kHz tap without a resampler. The tap stays private
   and uses `CATapMutedWhenTapped`, so original audio is muted only while eqtune reads it;
   stop the daemon and normal sound returns instantly.

2. **A private aggregate device** (`AudioHardwareCreateAggregateDevice`) that bundles one
   **snapshotted output device** (clock + playback) together with **our tap** (input).
   Putting both in one aggregate means they share a single clock — so there is no
   resampling or drift to fight between "what we captured" and "what we play back."

3. **An I/O callback** (`AudioDeviceCreateIOProcID` + `AudioDeviceStart`). Each cycle the
   `io_proc` copies the tapped system audio into the output buffer and then calls back into
   Rust (`eqtune_process_cb`) to equalize that block **in place**.

The resulting signal path:

```
system audio ─▶ device-scoped process tap (excludes eqtune; muted-when-tapped)
             ─▶ private aggregate device (output device + tap, one shared clock)
             ─▶ IOProc: capture → [Rust: preamp → biquad cascade → soft limiter] → replay
             ─▶ your current default output device
```

**Following the output device.** Startup resolves the default device ID once, enumerates
that exact device's output streams, and accepts one mixable interleaved stereo Float32
stream at any valid native rate. UID, name, nominal rate, and the selected stream format
all come from that snapshot; its UID and stream index are passed into tap construction.
The running target becomes authoritative only after aggregate validation and startup,
while a complete failed target remains visible in `status` for diagnosis. The daemon
polls every 100 ms; when you plug in headphones or switch to Bluetooth, it tears the
aggregate down and rebuilds it around one new snapshot (`follow_default_device`).

**Compatibility boundary.** The selected stream must be mixable, little-endian,
interleaved stereo Float32. Its native sample rate is unrestricted; the device-scoped tap
inherits it, the aggregate uses the output as its clock, and Rust designs the filters for
that same rate. This path is verified with 44.1 kHz Apple USB EarPods, 44.1 kHz Baseus
AirGo Bluetooth headphones, and 48 kHz MacBook Air speakers. Core Audio's
client-facing virtual format is what matters, so a USB device that physically transports
16- or 24-bit PCM can still qualify when the HAL exposes Float32 to clients.

Zero or multiple output streams, true mono or multichannel streams, non-interleaved or
integer client formats, nonmixable/big-endian PCM, and encoded passthrough are rejected
before unsafe processing. The aggregate is also revalidated before its IOProc starts.
There is intentionally no channel mapper, interleaver, integer converter, decoder, or
resampler in the realtime path; unsupported configurations keep native audio and expose
their topology or format through status and the daemon log.

---

## 5. The DSP, and the lock-free hand-off

`src/dsp.rs` is plain Rust with no OS dependencies. The EQ is a cascade of **biquad
filters** using the well-known RBJ "Audio EQ Cookbook" coefficients (peaking, low-shelf,
high-shelf), each implemented in Transposed Direct Form II for good floating-point
behavior. The per-sample path is: `preamp → biquad cascade → optional soft limiter`.

The interesting part is how settings cross the plane boundary safely. Two types:

- **`EqSettings`** — an *immutable* snapshot of everything the audio thread needs
  (designed coefficients, preamp gain, limiter flag, bypass endpoint). Built on the
  control thread.
- **`Processor`** — *audio-thread-local* filter state (the biquad memory).

They're connected by an `Arc<ArcSwap<EqSettings>>` (the `arc-swap` crate). The control
thread publishes a new snapshot with a single atomic pointer swap; the audio thread reads
the current snapshot each block with a wait-free `load()`. Each constructed snapshot also
carries a unique generation stamp, and `EqSettings`'s fields are private, so the audio
thread detects updates by value rather than by heap address — an allocator reusing an
`Arc` address cannot hide a fresh setting.

**No locks touch the audio thread.** This matters enormously: blocking or waiting on a
mutex inside a real-time audio callback risks priority inversion and audible dropouts. The
atomic-swap pattern means a live EQ edit is just "allocate a new `EqSettings`, swap the
pointer," and the next audio block picks it up cleanly. The processor also reserves
capacity for `MAX_BANDS` (64) per channel when it is created, and every mutation/import/load
path enforces that cap, so adopting a larger preset resizes within existing capacity
instead of allocating on the audio thread. Snapshots also retain each coefficient's source
band metadata. When an edit, insertion, or removal changes the band occupying a cascade
slot, that section's delay state is reset rather than being inherited by an unrelated band.

Bypass adds only three scalar values to `Processor`. During its 10 ms endpoint ramp the
existing loop mixes dry and wet samples; at either endpoint it avoids mix arithmetic.
The wet cascade still advances while output is fully dry, so switching back cannot revive
stale state. Bypass is runtime-only and leaves the tap active; `off` still drops
`TapSession` for the energy-saving native path.

The zero-dependency `benches/dsp.rs` executable measures steady-state, sustained silence,
settings adoption, the 64-band cap, and bypass transitions offline. No clock read or
timing counter is compiled into the production callback.

`src/sys.rs` wires this up: `process_trampoline` is the `extern "C"` function the shim
calls. It loads the current settings and runs the processor over the buffer. The
`TapSession` struct owns the native session and **stops audio on `Drop`** (RAII) — so
"turn eqtune off" is literally "drop the `TapSession`," and there's no way to leak the
Core Audio objects or stop them in the wrong order.

The IOProc also validates the live input/output buffer topology on every block. A mismatch
atomically publishes one fatal error and silences only that unsafe block; the control loop
observes it within its next tick and drops `TapSession`. Because
`CATapMutedWhenTapped` lasts only for the tap lifetime, teardown promptly restores native
audio instead of leaving eqtune producing zeroes indefinitely.

---

## 6. Engine lifecycle — the reconcile state machine

The daemon never starts/stops the engine ad hoc. Instead it keeps a small amount of
intent and *reconciles*:

- `engine_target_on` — whether the engine *should* be running right now.
- `user_intent` — your last explicit `on`/`off`, in memory. The automatic suspends (Low
  Power Mode, idle) gate on this. It is seeded at startup from the persisted
  `config.enabled` and updated the instant a command is handled — before the write that
  records it durably.
- `config.enabled` — the *persisted* on/off, restored at daemon startup so an enabled EQ
  survives a reboot or daemon restart. It mirrors disk; `user_intent` is what the live
  reconcile logic reads, so a failed persist (reported as a retryable error) can never
  desync the idle/LPM behavior from what is actually running.
- `low_power` — the last-seen macOS Low Power Mode state.
- `idle_suspended` — whether the engine is off because captured audio stayed silent.
- `recovery` — retry count, deadline, and exhaustion for the current failure incident.
- `last_engine_error` — the latest startup or runtime failure for diagnostics.

`reconcile()` simply makes reality match `engine_target_on`: start the engine if it should
be on and isn't, drop it if it should be off and is. Every event routes through this:

- Daemon startup seeds `user_intent` from the persisted `config.enabled` and reconciles
  once (respecting the Low-Power-Mode policy) before serving requests. When idle auto-off
  is enabled the restore is *lazy* — the engine starts suspended and `follow_idle_activity`
  starts it once playback is actually detected, so a login/restart with nothing playing
  never runs the tap through startup silence; with idle auto-off disabled there is no
  resume probe, so the restore is eager. A start failure is logged and starts bounded
  recovery rather than killing the daemon under launchd KeepAlive.
- `eqtune on` sets desired `user_intent`, tries immediately, and persists the intent even
  when that first start fails. Failure leaves native output active while retaining the
  explicit intent. `eqtune off`
  clears intent, cancels recovery, stops the engine first — unconditionally — then
  persists, so a failed config write can cost durability but never leaves audio processing
  running or lets a later reconcile restore the EQ.
- `follow_low_power()` (polled) detects a Low-Power-Mode edge: entering LPM forces the
  engine off (a large power saving) while remembering `user_intent`; leaving LPM
  restores it. An explicit `eqtune on` overrides and runs even under LPM.
- `follow_idle_activity()` watches the audio thread's silent-frame counter while the
  engine is running. After sustained silence it drops the engine; while suspended, it
  polls Core Audio's default-output-device activity and resumes when playback starts.
- `follow_default_device()` rebuilds a running engine when the target changes and also
  observes device-ID changes while the engine is down, so exhausted recovery can restart
  on a genuinely new output.
- A failed desired start schedules at most six retries after 1, 2, 4, 8, 16, and 30
  seconds. Exhaustion cannot be bypassed by ordinary reconciles. Only an explicit `on`,
  an output-device change, or a real Low-Power/idle policy resume resets the incident.

This is the same mechanism the energy work builds on (§7): "don't run the engine when we
don't need it."

---

## 7. Energy model

Because eqtune is an always-on daemon that processes **all** system audio in real time, it
inherently costs more power than Apple's native path — a long listening session on battery
will drain noticeably faster (see the README's *Battery & energy* section). The codebase
attacks this on two fronts:

- **Run the engine less.** Idle auto-off, Low Power Mode auto-off, and `eqtune off` tear
  the whole Core Audio pipeline down — the biggest lever.
- **Make each block cheaper.** The real-time `Processor` (a) re-copies filter coefficients
  only when the settings generation changes (steady-state blocks do zero coefficient
  work), (b) drops 0 dB "identity" bands at design time so they cost no biquad, and (c)
  skips per-sample processing entirely during sustained silence. Entering that skip state
  clears the biquad delay memory first, so a suspended filter tail cannot reappear when
  playback resumes.

Idle suspension uses captured silence while running and Core Audio output-device activity
while suspended. That keeps the implementation small, but it is still a practical proxy
for "media is streaming" rather than a per-app media-session API.

---

## 8. Persistence & packaging

- **Config** (`src/config.rs`) is TOML at `~/Library/Application Support/eqtune/config.toml`:
  named presets (each a list of bands + a preamp) plus global toggles (`enabled`,
  `limiter`, `auto_off_low_power`, `auto_off_idle`, …). It ships working defaults, so a
  first run needs no file. Loads validate every preset against the realtime engine's limits
  (finite values, practical ranges, and at most 64 bands). A malformed or unrunnable config
  is moved aside as `config.toml.corrupt` or `config.toml.corrupt.N` and eqtune continues
  from shipped defaults, preserving the bad file for manual recovery. Saves write a sibling
  temp file, fsync it, atomically rename it into place, and fsync the directory so a crash
  cannot easily truncate the live config. The daemon holds both a working config and the last saved config: live
  tuning edits affect only the working config until the `off` prompt resolves them.
  Preset-management commands mutate the saved preset map directly. `preset-save` can create
  a name, overwrite the active preset's own name, or overwrite a shipped preset, but it
  refuses to replace an unrelated custom preset. Deleting the last preset is rejected, and
  deleting the active preset selects another remaining preset before live settings are
  applied.
  `reset <name>` restores one shipped preset from `Config::default()`; `reset` without a
  name restores all shipped presets while preserving user-created presets and global
  toggles.
  Import/export use a smaller single-preset TOML format (`name`, `preamp_db`, and
  `bands`) for sharing settings without copying the whole config file. The CLI resolves
  relative import/export paths against the user's current directory before sending the
  request to the daemon; omitted export paths default to `<preset>.toml` in that directory.
  `preset-show [name]` reads the active or named preset from the working config without
  switching presets or writing anything, so it also reflects any live unsaved edits.

- **launchd** (`src/launchd.rs`) writes a LaunchAgent plist with `RunAtLoad` + `KeepAlive`
  so the daemon starts at login and is restarted if it dies. `eqtune install` stages the
  binary as a sibling temp file, ad-hoc signs that staged copy locally, atomically renames
  it into the stable daemon location, and verifies launchd reaches the running state after
  bootstrapping or restarting the agent. A healthy loaded agent still restarts with
  `launchctl kickstart -k`; if launchd keeps stale launch constraints and the restarted
  job fails to run, install falls back to bootout + bootstrap.
- **No Developer ID signing.** The installer uses local ad-hoc signing for the daemon
  copy, so no Apple Developer account, certificate, notarization, driver, or kernel
  extension is needed. `build.rs` embeds an `Info.plist` into the binary so macOS shows a
  proper audio-capture permission prompt.

### Session drafts and shipped presets

The daemon deliberately separates "what is playing now" from "what is saved":

- `config` is the working config. `band`, `band-rm`, and `preamp` mutate this copy and
  immediately push new `EqSettings` to the audio thread, but do not write the saved
  config. `preset` (a switch of `active_preset`) commits to the saved config immediately,
  so switching alone never counts as an unsaved session.
- `saved_config` mirrors the last config written to disk. It is the source for discard,
  save-as, and reset operations, so unrelated draft edits are not accidentally persisted.
- While `config != saved_config`, the working config is mirrored to a sibling
  `session.toml` (atomic temp-file + rename, but without the config's fsyncs: the mirror
  is best-effort, so durability-grade flushes in the single-threaded loop would buy
  nothing). Writes are rate-limited: the first edit after a quiet period mirrors
  immediately (an isolated edit is never at risk), but a burst within a short window is
  coalesced into one write flushed from the poll loop, so dragging a control doesn't
  rewrite the whole config on every step. The mirror is removed the moment the session
  resolves; if that removal fails, the resolving command reports it as an error, because
  a leftover draft would restore the just-resolved session at the next startup. At
  startup the daemon restores a leftover mirror as an unsaved draft — so a reboot,
  crash, or reinstall does not silently lose live edits, and the `off` prompt still
  decides their fate. Only preset *contents* are trusted from the mirror, preset by
  preset, for names the saved config knows — a legitimate draft only ever modifies
  existing presets, so a stale one can neither delete a just-saved preset nor smuggle
  one in. The active preset and global toggles always come from the saved config, since
  switches and toggle changes commit immediately and a stale draft must not revert them.
  An unreadable or unrunnable mirror is moved aside as `session.toml.corrupt` and ignored.
- `eqtune off` stops the audio engine first. If `config != saved_config`, the daemon
  returns `UnsavedSession` carrying the active tuning plus the names of every preset
  whose working contents differ from the saved config. Edits stay attached to the preset
  they were made on across preset switches, so those names can include presets other
  than the active one; the CLI names them instead of implying the active curve is all
  there is, and offers save-by-name only when the active preset itself has edits. EOF at
  any follow-up prompt is an error rather than an empty/default answer, so the CLI sends
  no save, overwrite, discard, or reset request and the mirrored draft remains available
  to resolve later.
- Save by name (`SaveSessionAs`) takes the active working preset and writes it into a
  clone of `saved_config`. If the name is unused, it creates a user preset. If the name is
  one of the shipped names (`bright`, `mellow`, `pro`) or the active preset's own name, it
  overwrites that preset — saving back into the preset being edited is the overwrite
  action, not a collision. Names of *other* custom presets are rejected to prevent
  accidental loss. The save consumes the active preset's edits (and, on an explicit
  overwrite, supersedes pending edits of the overwritten preset); unsaved edits on any
  other preset are carried into the new working config and stay an open session for the
  next `off` prompt, never silently reverted. `preset-clone`, like the other
  preset-management commands, is rejected while a session is open — it rebuilds the
  working config from the saved one, which would drop those edits.
- Overwrite (`SaveSessionOverwrite`) writes the entire working config as-is, preserving
  the active preset name. This is the direct path for "I tuned bright; make my device's
  bright sound like this now."
- Discard (`DiscardSession`) replaces the working config with `saved_config` and pushes
  those saved settings back to the audio handle if audio is running again later.
- Reset commands first reject unresolved live drafts, then compare existing target shipped
  preset(s) in `saved_config` against `Config::default()`. If a local shipped preset has
  been modified, the daemon returns `ResetWouldOverwrite`; the CLI warns and offers to
  save the current local version under a user preset name before confirming. Deleted
  shipped presets are recreated directly because there is no local version to preserve.
- `eqtune reset bright`, `eqtune reset mellow`, and `eqtune reset pro` replace exactly
  that preset with the shipped preset from `Config::default()`, recreating it if deleted.
- `eqtune reset` replaces all shipped presets from `Config::default()` and sets the active
  preset to the shipped default (`bright`), while preserving user-created presets.

---

## 9. Why a standalone CLI/daemon, not the "built-in macOS EQ"

**There is no built-in system-wide EQ on macOS to use.** The only first-party equalizer is
the graphic EQ *inside Music.app*, and it only affects Music's own playback — it does
nothing for Safari, Spotify, video, games, or system sounds. There is no setting anywhere
in macOS that equalizes the whole system mix.

The existing third-party options solve this the heavy way: they install a loopback or
kernel audio driver and **make themselves your default output device**, routing everything
through their virtual device. That breaks macOS's normal device-switching ("send audio to
the headphones when I plug them in"), needs kernel extensions and the signing/notarization
that entails, and is a lot of moving parts to trust with all your audio.

eqtune takes the opposite approach enabled by the new process-tap API: it **observes** the
system mix and replays to whatever your *current* output device is, so device switching
keeps working and no driver is needed. And the **daemon + CLI** shape is the natural fit
for that:

- The work is an always-on background service, so it wants a long-lived process (launchd),
  not a window you keep open.
- The control surface is small and benefits from being **scriptable and composable**
  (`eqtune band 2000 -6` in a shell, a keybinding, a Shortcut) — exactly what a CLI over a
  socket gives you, with room for a GUI to be layered on later as just another client.

---

## 10. Why Rust

- **Real-time safety without a garbage collector.** Audio callbacks have hard deadlines; a
  GC pause or an unexpected lock means an audible glitch. Rust gives predictable,
  no-pause performance, and its ownership model made the **lock-free `ArcSwap` hand-off**
  (§5) straightforward and provably free of data races at compile time.
- **Memory safety for a long-lived daemon.** A process that runs for weeks and juggles raw
  Core Audio handles is exactly where use-after-free and leaks hurt. Rust confines all
  `unsafe` to the thin `sys.rs` boundary; the rest of the code can't segfault.
- **RAII for native resources.** `TapSession`'s `Drop` tears the tap/aggregate/IOProc down
  in the correct order automatically — turning the engine off is just dropping a value.
- **Expressive protocol & config types.** `enum`s + `serde`/`toml` make the IPC protocol
  and the on-disk config robust and self-documenting, with cheap round-trip tests.
- **Great packaging story.** `cargo` builds the whole thing (shim included, via `build.rs`)
  with a tiny dependency set and an easy install-from-source path.

---

## 11. Why it can't be pure Rust — the Objective-C shim

The DSP, config, IPC, daemon, and lifecycle are **100% Rust**. The only Objective-C is
`shim/tap_shim.m`, and it exists because the system APIs eqtune depends on are only
practically reachable from Objective-C/C:

- **The process-tap API is brand-new and Objective-C-shaped.** `CATapDescription` is an
  Objective-C *class* you construct with an Objective-C initializer
  (`initStereoGlobalTapButExcludeProcesses:`); creating the aggregate device means building
  CoreFoundation/Foundation dictionaries and relying on toll-free bridging
  (`NSDictionary` ⇄ `CFDictionaryRef`, `NSArray` of boxed audio-object IDs). This is
  idiomatic Obj-C, not a flat C API.
- **There are no mature Rust bindings for it.** Because the API shipped in macOS 14.2,
  there's no crate that wraps it. Doing it in "pure" Rust would mean hand-writing Objective-C
  runtime message sends (`objc2`/`msg_send!`) and manual CoreFoundation bridging for a
  large, unfamiliar, fast-moving API surface — a lot of `unsafe`, easy to get subtly wrong,
  and painful to maintain.
- **A thin shim is simpler and safer.** ~250 lines of Objective-C, compiled with ARC
  (`-fobjc-arc`) so the Obj-C object lifetimes are managed for us, expose a **tiny, stable C
  ABI** in `shim/tap_shim.h`:

  ```c
  uint32_t eqtune_default_output_device(void);
  double   eqtune_default_output_sample_rate(void);
  bool     eqtune_low_power_enabled(void);            // Foundation's NSProcessInfo
  bool     eqtune_default_output_device_running(void);
  eqtune_tap_session *eqtune_tap_start(eqtune_process_cb cb, void *ctx);
  void     eqtune_tap_stop(eqtune_tap_session *session);
  ```

  Rust calls these functions through a small `extern "C"` block in `sys.rs`. The shim
  also gives us Low Power Mode detection (`NSProcessInfo.isLowPowerModeEnabled`) for free,
  since we're already in Foundation.

The division of labor is the point: **Objective-C owns only the system-API plumbing it's
uniquely good at; Rust owns all the logic.** The C ABI between them is small enough to read
at a glance and stable enough that the audio internals can change without touching Rust.

---

## 12. Threading & real-time safety, in one picture

```
control thread (daemon)                 real-time thread (Core Audio IOProc)
───────────────────────                 ────────────────────────────────────
parse Request                           load() current EqSettings   (wait-free)
mutate working Config                   copy coeffs only if changed  (cheap)
design new EqSettings                   skip work if silent          (cheap)
ArcSwap::store(Arc::new(settings)) ───▶  preamp → biquads → limiter   (in place)
save/discard draft on request
```

The only thing shared between the threads is the atomically-swapped `Arc<EqSettings>`. The
control thread never blocks the audio thread, and the audio thread never allocates, never
locks, and never calls back into the OS — which is exactly what a glitch-free system-wide
EQ requires.
