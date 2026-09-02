#ifndef EQTUNE_TAP_SHIM_H
#define EQTUNE_TAP_SHIM_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// AudioObjectID of the current default output device, or 0 on failure.
uint32_t eqtune_default_output_device(void);

// Nominal sample rate of exactly `dev` (Hz), or 0 on failure.
double eqtune_output_device_sample_rate(uint32_t dev);

// true when macOS Low Power Mode is currently enabled.
bool eqtune_low_power_enabled(void);

// true when the current default output device is running somewhere.
bool eqtune_default_output_device_running(void);

// Writes the name of output device `dev` as a NUL-terminated UTF-8 C string into `buf`
// (capacity `buflen` bytes). Returns false if `dev` is 0/unknown, its name can't be read,
// or `buf` is too small.
bool eqtune_output_device_name(uint32_t dev, char *buf, size_t buflen);

// Writes the stable UID of exactly `dev` as a NUL-terminated UTF-8 string.
bool eqtune_output_device_uid(uint32_t dev, char *buf, size_t buflen);

// The useful, fixed-size subset of an AudioStreamBasicDescription exposed to Rust.
typedef struct eqtune_stream_facts {
    double sample_rate;
    uint32_t format_id;
    uint32_t format_flags;
    uint32_t bytes_per_frame;
    uint32_t channels_per_frame;
    uint32_t bits_per_channel;
} eqtune_stream_facts;

// One coherent output-stream query. `stream_count` is always populated on a successful
// query; `stream_index` and `facts` are populated only when the device has exactly one
// output stream, which is eqtune's supported consumer-output topology.
typedef struct eqtune_output_stream {
    uint32_t stream_count;
    uint32_t stream_index;
    eqtune_stream_facts facts;
} eqtune_output_stream;

bool eqtune_output_device_stream(uint32_t dev, eqtune_output_stream *stream);

// Called from the real-time audio thread to process captured audio in place.
// `buffer` holds `frames * channels` interleaved 32-bit float samples.
typedef void (*eqtune_process_cb)(void *ctx, float *buffer, uint32_t frames, uint32_t channels);

// Opaque handle to a running capture→process→replay session.
typedef struct eqtune_tap_session eqtune_tap_session;

// Start: tap all system audio except this process, process each block via `cb`, and
// replay to the exact `output_device` through ONE private aggregate device
// (output device + tap share a clock, so no drift compensation is required).
// Returns NULL on failure (details are logged to stderr).
eqtune_tap_session *eqtune_tap_start(uint32_t output_device,
                                     uint32_t output_stream_index,
                                     eqtune_process_cb cb,
                                     void *ctx,
                                     char *error_buf,
                                     size_t error_buflen);

// A nonzero value means the realtime callback observed a fatal stream/layout change.
// The control thread must promptly stop the session; the callback never tears down Core
// Audio objects itself.
uint32_t eqtune_tap_runtime_error(eqtune_tap_session *session);

// Stop and tear down a session (safe to call with NULL).
void eqtune_tap_stop(eqtune_tap_session *session);

#ifdef __cplusplus
}
#endif

#endif // EQTUNE_TAP_SHIM_H
