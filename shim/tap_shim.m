#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/CATapDescription.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import "tap_shim.h"

#include <stdint.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void log_err(const char *what, OSStatus st) {
    fprintf(stderr, "eqtune shim: %s failed (OSStatus %d)\n", what, (int)st);
}

// Current default output device, or kAudioObjectUnknown (0) on failure. Defined in the
// helpers section below; the single query lives there so nothing here re-implements it.
static AudioObjectID default_output_device(void);
// Caller must CFRelease the returned string.
static CFStringRef copy_device_uid(AudioObjectID device);

static bool copy_device_stream_format(AudioDeviceID device,
                                      AudioObjectPropertyScope scope,
                               AudioStreamBasicDescription *format) {
    UInt32 size = sizeof(*format);
    AudioObjectPropertyAddress addr = {
        .mSelector = kAudioDevicePropertyStreamFormat,
        .mScope = scope,
        .mElement = kAudioObjectPropertyElementMain,
    };
    return AudioObjectGetPropertyData(device, &addr, 0, NULL, &size, format) == noErr;
}

static bool copy_audio_stream_format(AudioStreamID stream,
                                     AudioStreamBasicDescription *format) {
    UInt32 size = sizeof(*format);
    AudioObjectPropertyAddress addr = {
        .mSelector = kAudioStreamPropertyVirtualFormat,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    return AudioObjectGetPropertyData(stream, &addr, 0, NULL, &size, format) == noErr;
}

// The Rust processor consumes one interleaved stereo Float32 buffer. Establish that
// contract before the IOProc starts instead of reinterpreting an arbitrary hardware
// layout as floats in the real-time callback.
static bool supported_stereo_float_format(const AudioStreamBasicDescription *format) {
    return format->mFormatID == kAudioFormatLinearPCM &&
           (format->mFormatFlags & kAudioFormatFlagIsFloat) != 0 &&
           (format->mFormatFlags & kAudioFormatFlagIsBigEndian) == 0 &&
           (format->mFormatFlags & kAudioFormatFlagIsNonInterleaved) == 0 &&
           (format->mFormatFlags & kAudioFormatFlagIsNonMixable) == 0 &&
           format->mBitsPerChannel == 32 &&
           format->mChannelsPerFrame == 2 &&
           format->mBytesPerFrame == 2 * sizeof(float);
}

uint32_t eqtune_default_output_device(void) {
    return (uint32_t)default_output_device();
}

double eqtune_output_device_sample_rate(uint32_t dev_id) {
    AudioObjectID dev = (AudioObjectID)dev_id;
    if (dev == kAudioObjectUnknown) {
        return 0;
    }
    Float64 rate = 0;
    UInt32 rsize = sizeof(rate);
    AudioObjectPropertyAddress raddr = {
        .mSelector = kAudioDevicePropertyNominalSampleRate,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    if (AudioObjectGetPropertyData(dev, &raddr, 0, NULL, &rsize, &rate) != noErr) {
        return 0;
    }
    return (double)rate;
}

bool eqtune_low_power_enabled(void) {
    return [[NSProcessInfo processInfo] isLowPowerModeEnabled] ? true : false;
}

bool eqtune_default_output_device_running(void) {
    AudioObjectID dev = (AudioObjectID)eqtune_default_output_device();
    if (dev == kAudioObjectUnknown) {
        return false;
    }
    UInt32 running = 0;
    UInt32 size = sizeof(running);
    AudioObjectPropertyAddress addr = {
        .mSelector = kAudioDevicePropertyDeviceIsRunningSomewhere,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    OSStatus status = AudioObjectGetPropertyData(dev, &addr, 0, NULL, &size, &running);
    return status == noErr && running != 0;
}

bool eqtune_output_device_name(uint32_t dev_id, char *buf, size_t buflen) {
    if (!buf || buflen == 0) {
        return false;
    }
    @autoreleasepool {
        AudioObjectID dev = (AudioObjectID)dev_id;
        if (dev == kAudioObjectUnknown) {
            return false;
        }
        CFStringRef name = NULL;
        UInt32 size = sizeof(name);
        AudioObjectPropertyAddress addr = {
            .mSelector = kAudioObjectPropertyName,
            .mScope = kAudioObjectPropertyScopeGlobal,
            .mElement = kAudioObjectPropertyElementMain,
        };
        // The Name property returns a +1 CFStringRef the caller owns (like the UID helper below).
        if (AudioObjectGetPropertyData(dev, &addr, 0, NULL, &size, &name) != noErr || !name) {
            return false;
        }
        bool ok = CFStringGetCString(name, buf, (CFIndex)buflen, kCFStringEncodingUTF8);
        CFRelease(name);
        return ok;
    }
}

bool eqtune_output_device_uid(uint32_t dev_id, char *buf, size_t buflen) {
    if (!buf || buflen == 0) {
        return false;
    }
    @autoreleasepool {
        CFStringRef uid = copy_device_uid((AudioObjectID)dev_id);
        if (!uid) {
            return false;
        }
        bool ok = CFStringGetCString(uid, buf, (CFIndex)buflen, kCFStringEncodingUTF8);
        CFRelease(uid);
        return ok;
    }
}

static void export_stream_facts(const AudioStreamBasicDescription *format,
                                eqtune_stream_facts *facts) {
    facts->sample_rate = format->mSampleRate;
    facts->format_id = format->mFormatID;
    facts->format_flags = format->mFormatFlags;
    facts->bytes_per_frame = format->mBytesPerFrame;
    facts->channels_per_frame = format->mChannelsPerFrame;
    facts->bits_per_channel = format->mBitsPerChannel;
}

bool eqtune_output_device_stream(uint32_t dev_id, eqtune_output_stream *output) {
    if (!output || dev_id == kAudioObjectUnknown) {
        return false;
    }
    memset(output, 0, sizeof(*output));
    AudioObjectPropertyAddress addr = {
        .mSelector = kAudioDevicePropertyStreams,
        .mScope = kAudioObjectPropertyScopeOutput,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize((AudioDeviceID)dev_id, &addr, 0, NULL, &size) != noErr ||
        size % sizeof(AudioStreamID) != 0) {
        return false;
    }
    output->stream_count = size / sizeof(AudioStreamID);
    if (output->stream_count != 1) {
        return true;
    }

    AudioStreamID stream = kAudioObjectUnknown;
    if (AudioObjectGetPropertyData((AudioDeviceID)dev_id, &addr, 0, NULL, &size, &stream) != noErr ||
        stream == kAudioObjectUnknown) {
        return false;
    }
    AudioStreamBasicDescription format = {0};
    if (!copy_audio_stream_format(stream, &format)) {
        return false;
    }
    output->stream_index = 0;
    export_stream_facts(&format, &output->facts);
    return true;
}

// --- helpers ---------------------------------------------------------------

static AudioObjectID default_output_device(void) {
    AudioObjectID dev = kAudioObjectUnknown;
    UInt32 size = sizeof(dev);
    AudioObjectPropertyAddress addr = {
        .mSelector = kAudioHardwarePropertyDefaultOutputDevice,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    AudioObjectGetPropertyData(kAudioObjectSystemObject, &addr, 0, NULL, &size, &dev);
    return dev;
}

// Caller must CFRelease the returned string.
static CFStringRef copy_device_uid(AudioObjectID device) {
    CFStringRef uid = NULL;
    UInt32 size = sizeof(uid);
    AudioObjectPropertyAddress addr = {
        .mSelector = kAudioDevicePropertyDeviceUID,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    if (AudioObjectGetPropertyData(device, &addr, 0, NULL, &size, &uid) != noErr) {
        return NULL;
    }
    return uid;
}

// Our own process as an AudioObjectID, so we can exclude it from the tap (otherwise
// our replayed audio would be re-captured -> feedback loop).
static AudioObjectID self_process_object(void) {
    pid_t pid = getpid();
    AudioObjectID obj = kAudioObjectUnknown;
    UInt32 size = sizeof(obj);
    AudioObjectPropertyAddress addr = {
        .mSelector = kAudioHardwarePropertyTranslatePIDToProcessObject,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    AudioObjectGetPropertyData(kAudioObjectSystemObject, &addr, sizeof(pid), &pid, &size, &obj);
    return obj;
}

// --- session ---------------------------------------------------------------

struct eqtune_tap_session {
    AudioObjectID tap;
    AudioDeviceID aggregate;
    AudioDeviceIOProcID ioproc;
    eqtune_process_cb cb;
    void *ctx;
    _Atomic uint32_t runtime_error;
};

enum {
    EQTUNE_RUNTIME_ERROR_NONE = 0,
    EQTUNE_RUNTIME_ERROR_OUTPUT_LAYOUT = 1,
    EQTUNE_RUNTIME_ERROR_INPUT_LAYOUT = 2,
    EQTUNE_RUNTIME_ERROR_BUFFER_SIZE = 3,
};

static void silence_output(AudioBufferList *output) {
    if (!output) {
        return;
    }
    for (UInt32 b = 0; b < output->mNumberBuffers; b++) {
        AudioBuffer *buffer = &output->mBuffers[b];
        if (buffer->mData) {
            memset(buffer->mData, 0, buffer->mDataByteSize);
        }
    }
}

static void fail_runtime(struct eqtune_tap_session *session,
                         AudioBufferList *output,
                         uint32_t error) {
    uint32_t expected = EQTUNE_RUNTIME_ERROR_NONE;
    atomic_compare_exchange_strong_explicit(&session->runtime_error, &expected, error,
                                            memory_order_relaxed, memory_order_relaxed);
    silence_output(output);
}

static OSStatus io_proc(AudioObjectID inDevice,
                        const AudioTimeStamp *inNow,
                        const AudioBufferList *inInputData,
                        const AudioTimeStamp *inInputTime,
                        AudioBufferList *outOutputData,
                        const AudioTimeStamp *inOutputTime,
                        void *inClientData) {
    (void)inDevice; (void)inNow; (void)inInputTime; (void)inOutputTime;
    struct eqtune_tap_session *s = (struct eqtune_tap_session *)inClientData;
    if (!outOutputData) {
        return noErr;
    }

    if (atomic_load_explicit(&s->runtime_error, memory_order_relaxed) !=
        EQTUNE_RUNTIME_ERROR_NONE) {
        silence_output(outOutputData);
        return noErr;
    }

    // Start-up validation establishes a single interleaved stereo stream. Keep a runtime
    // guard as well: Core Audio must never make us cast or copy an unexpected layout if a
    // device changes its stream topology underneath the aggregate.
    if (outOutputData->mNumberBuffers != 1 ||
        !outOutputData->mBuffers[0].mData ||
        outOutputData->mBuffers[0].mNumberChannels != 2) {
        fail_runtime(s, outOutputData, EQTUNE_RUNTIME_ERROR_OUTPUT_LAYOUT);
        return noErr;
    }

    AudioBuffer *out = &outOutputData->mBuffers[0];
    const UInt32 frame_bytes = 2 * sizeof(float);
    if (out->mDataByteSize == 0 || out->mDataByteSize % frame_bytes != 0) {
        fail_runtime(s, outOutputData, EQTUNE_RUNTIME_ERROR_BUFFER_SIZE);
        return noErr;
    }
    if (!inInputData || inInputData->mNumberBuffers != 1 ||
        !inInputData->mBuffers[0].mData ||
        inInputData->mBuffers[0].mNumberChannels != 2) {
        fail_runtime(s, outOutputData, EQTUNE_RUNTIME_ERROR_INPUT_LAYOUT);
        return noErr;
    }
    const AudioBuffer *in = &inInputData->mBuffers[0];
    if (in->mDataByteSize != out->mDataByteSize || in->mDataByteSize % frame_bytes != 0) {
        fail_runtime(s, outOutputData, EQTUNE_RUNTIME_ERROR_BUFFER_SIZE);
        return noErr;
    }

    memcpy(out->mData, in->mData, out->mDataByteSize);
    UInt32 frames = out->mDataByteSize / frame_bytes;
    if (s->cb && frames > 0) {
        s->cb(s->ctx, (float *)out->mData, frames, 2);
    }
    return noErr;
}

static void set_start_error(char *buf, size_t buflen, const char *message) {
    if (buf && buflen > 0) {
        snprintf(buf, buflen, "%s", message);
    }
}

static void set_start_osstatus_error(char *buf, size_t buflen, const char *what, OSStatus status) {
    if (buf && buflen > 0) {
        snprintf(buf, buflen, "%s failed (OSStatus %d)", what, (int)status);
    }
}

static void describe_format(const AudioStreamBasicDescription *format,
                            char *buf,
                            size_t buflen) {
    snprintf(buf, buflen,
             "%.0f Hz, %u ch, format 0x%08x, flags 0x%08x, %u-bit, %u B/frame",
             format->mSampleRate,
             (unsigned)format->mChannelsPerFrame,
             (unsigned)format->mFormatID,
             (unsigned)format->mFormatFlags,
             (unsigned)format->mBitsPerChannel,
             (unsigned)format->mBytesPerFrame);
}

static void set_start_format_error(char *buf,
                                   size_t buflen,
                                   const char *reason,
                                   const AudioStreamBasicDescription *input,
                                   const AudioStreamBasicDescription *output) {
    char input_desc[160];
    char output_desc[160];
    describe_format(input, input_desc, sizeof(input_desc));
    describe_format(output, output_desc, sizeof(output_desc));
    fprintf(stderr, "eqtune shim: %s: input [%s], output [%s]\n",
            reason, input_desc, output_desc);
    if (buf && buflen > 0) {
        snprintf(buf, buflen, "%s: input [%s], output [%s]",
                 reason, input_desc, output_desc);
    }
}

eqtune_tap_session *eqtune_tap_start(uint32_t output_device,
                                     uint32_t output_stream_index,
                                     eqtune_process_cb cb,
                                     void *ctx,
                                     char *error_buf,
                                     size_t error_buflen) {
    @autoreleasepool {
        AudioObjectID output = (AudioObjectID)output_device;
        if (output == kAudioObjectUnknown) {
            set_start_error(error_buf, error_buflen, "no output device");
            return NULL;
        }
        CFStringRef output_uid = copy_device_uid(output);
        if (!output_uid) {
            set_start_error(error_buf, error_buflen, "could not read output device UID");
            return NULL;
        }

        // Bind capture to the same snapshotted device stream used for playback. Core
        // Audio documents that this tap adopts that stream's format, avoiding a generic
        // global tap rate/layout that can disagree with (for example) 44.1 kHz USB audio.
        AudioObjectID self_obj = self_process_object();
        NSArray *exclude = (self_obj != kAudioObjectUnknown) ? @[ @(self_obj) ] : @[];
        CATapDescription *desc = [[CATapDescription alloc]
            initExcludingProcesses:exclude
            andDeviceUID:(__bridge NSString *)output_uid
            withStream:(NSInteger)output_stream_index];
        if (!desc) {
            set_start_error(error_buf, error_buflen,
                            "could not describe the selected output stream tap");
            CFRelease(output_uid);
            return NULL;
        }
        desc.name = @"eqtune";
        desc.privateTap = YES;
        desc.muteBehavior = CATapMutedWhenTapped;
        NSString *tap_uuid = desc.UUID.UUIDString;
        if (!tap_uuid) {
            set_start_error(error_buf, error_buflen, "could not create a tap UID");
            CFRelease(output_uid);
            return NULL;
        }

        AudioObjectID tap = kAudioObjectUnknown;
        OSStatus st = AudioHardwareCreateProcessTap(desc, &tap);
        if (st != noErr || tap == kAudioObjectUnknown) {
            log_err("AudioHardwareCreateProcessTap", st);
            set_start_osstatus_error(error_buf, error_buflen,
                                     "AudioHardwareCreateProcessTap", st);
            CFRelease(output_uid);
            return NULL;
        }

        // Aggregate device: the real output device (clock + playback) + our tap (input).
        NSString *agg_uid = [@"eqtune-aggregate-" stringByAppendingString:tap_uuid];
        NSDictionary *agg_desc = @{
            @(kAudioAggregateDeviceNameKey): @"eqtune",
            @(kAudioAggregateDeviceUIDKey): agg_uid,
            @(kAudioAggregateDeviceMainSubDeviceKey): (__bridge NSString *)output_uid,
            @(kAudioAggregateDeviceIsPrivateKey): @YES,
            @(kAudioAggregateDeviceSubDeviceListKey): @[
                @{ @(kAudioSubDeviceUIDKey): (__bridge NSString *)output_uid },
            ],
            @(kAudioAggregateDeviceTapListKey): @[
                @{ @(kAudioSubTapUIDKey): tap_uuid },
            ],
            @(kAudioAggregateDeviceTapAutoStartKey): @YES,
        };

        AudioDeviceID aggregate = kAudioObjectUnknown;
        st = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)agg_desc, &aggregate);
        CFRelease(output_uid);
        if (st != noErr || aggregate == kAudioObjectUnknown) {
            log_err("AudioHardwareCreateAggregateDevice", st);
            set_start_osstatus_error(error_buf, error_buflen,
                                     "AudioHardwareCreateAggregateDevice", st);
            AudioHardwareDestroyProcessTap(tap);
            return NULL;
        }


        AudioStreamBasicDescription input_format = {0};
        AudioStreamBasicDescription output_format = {0};
        bool input_read = copy_device_stream_format(aggregate, kAudioObjectPropertyScopeInput,
                                                    &input_format);
        bool output_read = copy_device_stream_format(aggregate, kAudioObjectPropertyScopeOutput,
                                                     &output_format);
        if (!input_read || !output_read) {
            const char *message = !input_read && !output_read
                ? "could not read aggregate input or output format"
                : (!input_read ? "could not read aggregate input format"
                               : "could not read aggregate output format");
            fprintf(stderr, "eqtune shim: %s\n", message);
            set_start_error(error_buf, error_buflen, message);
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            return NULL;
        }
        if (!supported_stereo_float_format(&input_format) ||
            !supported_stereo_float_format(&output_format)) {
            set_start_format_error(error_buf, error_buflen,
                                   "aggregate requires interleaved stereo Float32",
                                   &input_format, &output_format);
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            return NULL;
        }
        if (input_format.mSampleRate != output_format.mSampleRate) {
            set_start_format_error(error_buf, error_buflen,
                                   "aggregate input/output rates do not match",
                                   &input_format, &output_format);
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            return NULL;
        }

        struct eqtune_tap_session *s = calloc(1, sizeof(struct eqtune_tap_session));
        if (!s) {
            fprintf(stderr, "eqtune shim: could not allocate tap session\n");
            set_start_error(error_buf, error_buflen, "could not allocate tap session");
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            return NULL;
        }
        s->tap = tap;
        s->aggregate = aggregate;
        s->cb = cb;
        s->ctx = ctx;

        st = AudioDeviceCreateIOProcID(aggregate, io_proc, s, &s->ioproc);
        if (st != noErr) {
            log_err("AudioDeviceCreateIOProcID", st);
            set_start_osstatus_error(error_buf, error_buflen, "AudioDeviceCreateIOProcID", st);
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            free(s);
            return NULL;
        }

        st = AudioDeviceStart(aggregate, s->ioproc);
        if (st != noErr) {
            log_err("AudioDeviceStart", st);
            set_start_osstatus_error(error_buf, error_buflen, "AudioDeviceStart", st);
            AudioDeviceDestroyIOProcID(aggregate, s->ioproc);
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            free(s);
            return NULL;
        }

        return s;
    }
}

uint32_t eqtune_tap_runtime_error(eqtune_tap_session *s) {
    if (!s) {
        return EQTUNE_RUNTIME_ERROR_NONE;
    }
    return atomic_load_explicit(&s->runtime_error, memory_order_relaxed);
}

void eqtune_tap_stop(eqtune_tap_session *s) {
    if (!s) {
        return;
    }
    if (s->ioproc) {
        AudioDeviceStop(s->aggregate, s->ioproc);
        AudioDeviceDestroyIOProcID(s->aggregate, s->ioproc);
    }
    if (s->aggregate != kAudioObjectUnknown) {
        AudioHardwareDestroyAggregateDevice(s->aggregate);
    }
    if (s->tap != kAudioObjectUnknown) {
        AudioHardwareDestroyProcessTap(s->tap);
    }
    free(s);
}
