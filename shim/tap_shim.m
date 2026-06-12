#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/CATapDescription.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import "tap_shim.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void log_err(const char *what, OSStatus st) {
    fprintf(stderr, "eqtune shim: %s failed (OSStatus %d)\n", what, (int)st);
}

uint32_t eqtune_default_output_device(void) {
    AudioObjectID device = kAudioObjectUnknown;
    UInt32 size = sizeof(device);
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioHardwarePropertyDefaultOutputDevice,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    OSStatus status = AudioObjectGetPropertyData(
        kAudioObjectSystemObject, &address, 0, NULL, &size, &device);
    if (status != noErr) {
        return 0;
    }
    return (uint32_t)device;
}

double eqtune_default_output_sample_rate(void) {
    AudioObjectID dev = kAudioObjectUnknown;
    UInt32 dsize = sizeof(dev);
    AudioObjectPropertyAddress daddr = {
        .mSelector = kAudioHardwarePropertyDefaultOutputDevice,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &daddr, 0, NULL, &dsize, &dev) != noErr
        || dev == kAudioObjectUnknown) {
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
};

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

    for (UInt32 b = 0; b < outOutputData->mNumberBuffers; b++) {
        AudioBuffer *out = &outOutputData->mBuffers[b];
        float *out_data = (float *)out->mData;
        UInt32 channels = out->mNumberChannels ? out->mNumberChannels : 1;
        UInt32 frames = out->mDataByteSize / sizeof(float) / channels;

        // Fill the output from the matching tap input buffer (system audio).
        if (inInputData && b < inInputData->mNumberBuffers && inInputData->mBuffers[b].mData) {
            const AudioBuffer *in = &inInputData->mBuffers[b];
            UInt32 copy = in->mDataByteSize < out->mDataByteSize ? in->mDataByteSize : out->mDataByteSize;
            memcpy(out_data, in->mData, copy);
            if (copy < out->mDataByteSize) {
                memset((uint8_t *)out_data + copy, 0, out->mDataByteSize - copy);
            }
        } else {
            memset(out_data, 0, out->mDataByteSize);
        }

        if (s->cb && out_data && frames > 0) {
            s->cb(s->ctx, out_data, frames, channels);
        }
    }
    return noErr;
}

eqtune_tap_session *eqtune_tap_start(eqtune_process_cb cb, void *ctx) {
    @autoreleasepool {
        AudioObjectID output = default_output_device();
        if (output == kAudioObjectUnknown) {
            fprintf(stderr, "eqtune shim: no default output device\n");
            return NULL;
        }
        CFStringRef output_uid = copy_device_uid(output);
        if (!output_uid) {
            fprintf(stderr, "eqtune shim: could not read output device UID\n");
            return NULL;
        }

        // Tap: stereo, global, excluding ourselves; muted at the hardware only while
        // we are reading it, so stopping the daemon restores normal audio.
        AudioObjectID self_obj = self_process_object();
        NSArray *exclude = (self_obj != kAudioObjectUnknown) ? @[ @(self_obj) ] : @[];
        CATapDescription *desc = [[CATapDescription alloc] initStereoGlobalTapButExcludeProcesses:exclude];
        desc.name = @"eqtune";
        desc.privateTap = YES;
        desc.muteBehavior = CATapMutedWhenTapped;
        NSString *tap_uuid = desc.UUID.UUIDString;

        AudioObjectID tap = kAudioObjectUnknown;
        OSStatus st = AudioHardwareCreateProcessTap(desc, &tap);
        if (st != noErr || tap == kAudioObjectUnknown) {
            log_err("AudioHardwareCreateProcessTap", st);
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
            AudioHardwareDestroyProcessTap(tap);
            return NULL;
        }

        struct eqtune_tap_session *s = calloc(1, sizeof(struct eqtune_tap_session));
        s->tap = tap;
        s->aggregate = aggregate;
        s->cb = cb;
        s->ctx = ctx;

        st = AudioDeviceCreateIOProcID(aggregate, io_proc, s, &s->ioproc);
        if (st != noErr) {
            log_err("AudioDeviceCreateIOProcID", st);
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            free(s);
            return NULL;
        }

        st = AudioDeviceStart(aggregate, s->ioproc);
        if (st != noErr) {
            log_err("AudioDeviceStart", st);
            AudioDeviceDestroyIOProcID(aggregate, s->ioproc);
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            free(s);
            return NULL;
        }

        return s;
    }
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
