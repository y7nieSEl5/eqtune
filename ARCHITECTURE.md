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
   system audio ─▶ global tap ─▶ aggregate device ─▶ IOProc ─▶ default output device
                                         (capture → EQ → replay, one shared clock)
```

The same executable becomes the **client** or the **daemon** depending on the
subcommand: `eqtune daemon` (hidden, launched by launchd) runs the long-lived process;
every other command (`on`, `off`, `band`, `preset`, …) is a thin client that opens the
socket, sends **one** request, prints the reply, and exits.

### Two planes

It helps to think of eqtune as two independent planes:

- **Control plane** — how you talk to it. A Unix-domain socket carrying newline-delimited
  JSON. Low frequency, human-driven, never touches the audio thread directly.
- **Audio plane** — the real-time loop. A Core Audio I/O callback that fires hundreds of
  times a second and must never block. The control plane hands it new settings
  *lock-free* so a live EQ edit never stalls or glitches playback.

Keeping these planes decoupled is the central design idea, and it's what makes "edit the
EQ while music is playing, with no click" work.

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
enum Request  { Status, Enable, Disable, ListPresets, SetPreset(String),
                SetBand { freq, gain_db, q }, RemoveBand { freq },
                SetPreamp(f32), SetAutoOffLowPower(bool), Reset }
enum Response { Ok, Status(Status), Tuning(Tuning), Presets { … }, Error(String) }
```

A client (`eqtune band 2000 -6`) serializes one `Request` to JSON, writes a single line
to `~/Library/Application Support/eqtune/eqtune.sock`, and reads one `Response` line back.
The daemon's accept loop (`Daemon::run`) handles each connection, deserializes the
request, mutates state, and replies. `Enable` and the EQ edits reply with `Tuning` (the
active preset, preamp, and bands) so the CLI can print the resulting curve; `Disable` and
`SetAutoOffLowPower` reply `Ok` and the client renders the confirmation.

Because the wire format is "one JSON line in, one JSON line out," the protocol is trivial
to extend (add an enum variant) and trivial to test (`serde_json` round-trip tests live in
`ipc.rs`). There's no long-lived connection, no streaming, no versioning headache — the
client is stateless and the daemon is the single source of truth.

**Live edits.** Mutating commands (`SetBand`, `SetPreamp`, `SetPreset`, …) call
`persist_and_apply`: it writes the new config to disk *and*, if the engine is running,
pushes freshly-designed coefficients to the audio thread via `EqHandle::store` — without
restarting playback. (How that's lock-free is §5.)

---

## 4. Audio plane — the capture → EQ → replay loop

This is the part that needs Apple's frameworks, and it lives in `shim/tap_shim.m`. The
modern **Core Audio process-tap API** (macOS 14.2+) lets a normal user-space process
observe the system audio mix with no driver. eqtune sets up three objects:

1. **A global process tap** (`AudioHardwareCreateProcessTap` with a `CATapDescription`).
   It's a *stereo, global, private* tap that **excludes eqtune's own process** — otherwise
   the audio we replay would be re-captured into a feedback loop. (We find our own audio
   object via `kAudioHardwarePropertyTranslatePIDToProcessObject`.) It uses
   `CATapMutedWhenTapped`, so the original audio is muted *only while we're tapping it*;
   stop the daemon and normal sound returns instantly.

2. **A private aggregate device** (`AudioHardwareCreateAggregateDevice`) that bundles the
   **current default output device** (clock + playback) together with **our tap** (input).
   Putting both in one aggregate means they share a single clock — so there is no
   resampling or drift to fight between "what we captured" and "what we play back."

3. **An I/O callback** (`AudioDeviceCreateIOProcID` + `AudioDeviceStart`). Each cycle the
   `io_proc` copies the tapped system audio into the output buffer and then calls back into
   Rust (`eqtune_process_cb`) to equalize that block **in place**.

The resulting signal path:

```
system audio ─▶ global process tap (excludes eqtune; muted-when-tapped)
             ─▶ private aggregate device (output device + tap, one shared clock)
             ─▶ IOProc: capture → [Rust: preamp → biquad cascade → soft limiter] → replay
             ─▶ your current default output device
```

**Following the output device.** The daemon polls (every 100 ms) for the default output
device and its sample rate; when you plug in headphones or switch to Bluetooth, it tears
the aggregate down and rebuilds it around the new device (`follow_default_device`). That's
why "switch to the headphones when I plug them in" keeps working — eqtune never *becomes*
your output device, it follows whatever your output device currently is.

---

## 5. The DSP, and the lock-free hand-off

`src/dsp.rs` is plain Rust with no OS dependencies. The EQ is a cascade of **biquad
filters** using the well-known RBJ "Audio EQ Cookbook" coefficients (peaking, low-shelf,
high-shelf), each implemented in Transposed Direct Form II for good floating-point
behavior. The per-sample path is: `preamp → biquad cascade → optional soft limiter`.

The interesting part is how settings cross the plane boundary safely. Two types:

- **`EqSettings`** — an *immutable* snapshot of everything the audio thread needs
  (designed coefficients, preamp gain, limiter flag). Built on the control thread.
- **`Processor`** — *audio-thread-local* filter state (the biquad memory).

They're connected by an `Arc<ArcSwap<EqSettings>>` (the `arc-swap` crate). The control
thread publishes a new snapshot with a single atomic pointer swap; the audio thread reads
the current snapshot each block with a wait-free `load()`. **No locks touch the audio
thread.** This matters enormously: blocking or waiting on a mutex inside a real-time audio
callback risks priority inversion and audible dropouts. The atomic-swap pattern means a
live EQ edit is just "allocate a new `EqSettings`, swap the pointer," and the next audio
block picks it up cleanly.

`src/sys.rs` wires this up: `process_trampoline` is the `extern "C"` function the shim
calls. It loads the current settings and runs the processor over the buffer. The
`TapSession` struct owns the native session and **stops audio on `Drop`** (RAII) — so
"turn eqtune off" is literally "drop the `TapSession`," and there's no way to leak the
Core Audio objects or stop them in the wrong order.

---

## 6. Engine lifecycle — the reconcile state machine

The daemon never starts/stops the engine ad hoc. Instead it keeps a small amount of
intent and *reconciles*:

- `engine_target_on` — whether the engine *should* be running right now.
- `user_intent` — your last explicit `on`/`off`, remembered across an automatic suspend.
- `low_power` — the last-seen macOS Low Power Mode state.

`reconcile()` simply makes reality match `engine_target_on`: start the engine if it should
be on and isn't, drop it if it should be off and is. Every event routes through this:

- `eqtune on` / `off` set `user_intent` + `engine_target_on`, then reconcile.
- `follow_low_power()` (polled) detects a Low-Power-Mode edge: entering LPM forces the
  engine off (a large power saving) while remembering `user_intent`; leaving LPM restores
  it. An explicit `eqtune on` overrides and runs even under LPM.
- `follow_default_device()` rebuilds the running engine when the output device changes.

This is the same mechanism the energy work builds on (§7): "don't run the engine when we
don't need it."

---

## 7. Energy model

Because eqtune is an always-on daemon that processes **all** system audio in real time, it
inherently costs more power than Apple's native path — a long listening session on battery
will drain noticeably faster (see the README's *Battery & energy* section). The codebase
attacks this on two fronts:

- **Run the engine less.** Auto-off in Low Power Mode and on `eqtune off` tear the whole
  Core Audio pipeline down — the biggest lever.
- **Make each block cheaper.** The real-time `Processor` (a) re-copies filter coefficients
  only when the settings pointer actually changes (steady-state blocks do zero coefficient
  work), (b) drops 0 dB "identity" bands at design time so they cost no biquad, and (c)
  skips per-sample processing entirely during sustained silence.

The largest remaining win — fully suspending the engine whenever *nothing at all* is
playing (via a Core Audio "is anything playing" listener) — is future work, and the
reconcile machine in §6 is built to extend into it.

---

## 8. Persistence & packaging

- **Config** (`src/config.rs`) is TOML at `~/Library/Application Support/eqtune/config.toml`:
  named presets (each a list of bands + a preamp) plus global toggles (`limiter`,
  `auto_off_low_power`, …). It ships working defaults, so a first run needs no file.
- **launchd** (`src/launchd.rs`) writes a LaunchAgent plist with `RunAtLoad` + `KeepAlive`
  so the daemon starts at login and is restarted if it dies. `eqtune install` copies the
  binary to a stable location and bootstraps the agent.
- **No code signing.** Locally-built binaries aren't quarantined, so Gatekeeper never
  applies. `build.rs` embeds an `Info.plist` into the binary so macOS shows a proper
  audio-capture permission prompt without an Apple Developer account.

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
  eqtune_tap_session *eqtune_tap_start(eqtune_process_cb cb, void *ctx);
  void     eqtune_tap_stop(eqtune_tap_session *session);
  ```

  Rust calls these five functions through a small `extern "C"` block in `sys.rs`. The shim
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
mutate Config                           copy coeffs only if changed  (cheap)
design new EqSettings                   skip work if silent          (cheap)
ArcSwap::store(Arc::new(settings)) ───▶  preamp → biquads → limiter   (in place)
save TOML to disk
```

The only thing shared between the threads is the atomically-swapped `Arc<EqSettings>`. The
control thread never blocks the audio thread, and the audio thread never allocates, never
locks, and never calls back into the OS — which is exactly what a glitch-free system-wide
EQ requires.
