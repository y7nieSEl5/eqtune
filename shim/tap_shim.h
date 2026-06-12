#ifndef EQTUNE_TAP_SHIM_H
#define EQTUNE_TAP_SHIM_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// AudioObjectID of the current default output device, or 0 on failure.
uint32_t eqtune_default_output_device(void);

// Nominal sample rate of the current default output device (Hz), or 0 on failure.
double eqtune_default_output_sample_rate(void);

// true when macOS Low Power Mode is currently enabled.
bool eqtune_low_power_enabled(void);

// Called from the real-time audio thread to process captured audio in place.
// `buffer` holds `frames * channels` interleaved 32-bit float samples.
typedef void (*eqtune_process_cb)(void *ctx, float *buffer, uint32_t frames, uint32_t channels);

// Opaque handle to a running capture→process→replay session.
typedef struct eqtune_tap_session eqtune_tap_session;

// Start: tap all system audio except this process, process each block via `cb`, and
// replay to the current default output device through ONE private aggregate device
// (output device + tap share a clock, so no drift compensation is required).
// Returns NULL on failure (details are logged to stderr).
eqtune_tap_session *eqtune_tap_start(eqtune_process_cb cb, void *ctx);

// Stop and tear down a session (safe to call with NULL).
void eqtune_tap_stop(eqtune_tap_session *session);

#ifdef __cplusplus
}
#endif

#endif // EQTUNE_TAP_SHIM_H
