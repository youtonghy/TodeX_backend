// AVFoundation half of the agent camera on macOS (see `macos.rs`).
//
// The daemon polls: an open camera keeps only its newest frame, and
// `todex_camera_copy_frame` copies it out as tightly packed BGRA. Nothing
// calls back into Rust.

#import <AVFoundation/AVFoundation.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <os/lock.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

static void write_error(char *out, size_t len, NSString *message) {
    if (out == NULL || len == 0) {
        return;
    }
    const char *text = message.UTF8String ?: "unknown error";
    strlcpy(out, text, len);
}

@interface TodexCameraSink : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
@end

@implementation TodexCameraSink {
    os_unfair_lock _lock;
    CVPixelBufferRef _latest;
    uint64_t _seq;
}

- (instancetype)init {
    if ((self = [super init])) {
        _lock = OS_UNFAIR_LOCK_INIT;
    }
    return self;
}

- (void)dealloc {
    if (_latest != NULL) {
        CVPixelBufferRelease(_latest);
    }
}

- (void)captureOutput:(AVCaptureOutput *)output
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
           fromConnection:(AVCaptureConnection *)connection {
    CVImageBufferRef pixels = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (pixels == NULL) {
        return;
    }
    CVPixelBufferRetain(pixels);
    os_unfair_lock_lock(&_lock);
    CVPixelBufferRef previous = _latest;
    _latest = pixels;
    _seq += 1;
    os_unfair_lock_unlock(&_lock);
    if (previous != NULL) {
        CVPixelBufferRelease(previous);
    }
}

// The newest frame (retained) and its sequence number.
- (CVPixelBufferRef)copyLatest:(uint64_t *)seq {
    os_unfair_lock_lock(&_lock);
    CVPixelBufferRef pixels = _latest;
    if (pixels != NULL) {
        CVPixelBufferRetain(pixels);
    }
    *seq = _seq;
    os_unfair_lock_unlock(&_lock);
    return pixels;
}
@end

@interface TodexCamera : NSObject
@property(nonatomic, strong) AVCaptureSession *session;
@property(nonatomic, strong) TodexCameraSink *sink;
@property(nonatomic, strong) dispatch_queue_t queue;
@end

@implementation TodexCamera
@end

static NSArray<AVCaptureDevice *> *video_devices(void) {
    NSMutableArray<AVCaptureDeviceType> *types =
        [NSMutableArray arrayWithObject:AVCaptureDeviceTypeBuiltInWideAngleCamera];
    if (@available(macOS 14.0, *)) {
        [types addObject:AVCaptureDeviceTypeExternal];
        [types addObject:AVCaptureDeviceTypeContinuityCamera];
    } else {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        [types addObject:AVCaptureDeviceTypeExternalUnknown];
#pragma clang diagnostic pop
    }
    AVCaptureDeviceDiscoverySession *discovery =
        [AVCaptureDeviceDiscoverySession discoverySessionWithDeviceTypes:types
                                                               mediaType:AVMediaTypeVideo
                                                                position:AVCaptureDevicePositionUnspecified];
    return discovery.devices;
}

// JSON `[{"id": ..., "name": ...}]`, freed with `todex_camera_free`.
char *todex_camera_devices(void) {
    @autoreleasepool {
        NSMutableArray *list = [NSMutableArray array];
        for (AVCaptureDevice *device in video_devices()) {
            [list addObject:@{@"id" : device.uniqueID ?: @"", @"name" : device.localizedName ?: @""}];
        }
        NSData *json = [NSJSONSerialization dataWithJSONObject:list options:0 error:nil];
        if (json == nil) {
            return NULL;
        }
        char *out = malloc(json.length + 1);
        if (out == NULL) {
            return NULL;
        }
        memcpy(out, json.bytes, json.length);
        out[json.length] = 0;
        return out;
    }
}

// AVAuthorizationStatus: 0 not determined, 1 restricted, 2 denied, 3 authorized.
int32_t todex_camera_authorization(void) {
    return (int32_t)[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo];
}

// Shows the OS prompt when undecided and waits for the answer (at most
// `timeout_seconds`); returns the status afterwards.
int32_t todex_camera_request_access(double timeout_seconds) {
    if ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo] ==
        AVAuthorizationStatusNotDetermined) {
        dispatch_semaphore_t answered = dispatch_semaphore_create(0);
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo
                                 completionHandler:^(BOOL granted) {
                                   (void)granted;
                                   dispatch_semaphore_signal(answered);
                                 }];
        dispatch_semaphore_wait(answered,
                                dispatch_time(DISPATCH_TIME_NOW, (int64_t)(timeout_seconds * NSEC_PER_SEC)));
    }
    return todex_camera_authorization();
}

// Starts the camera `device_id`, delivering frames at most `max_width`
// wide. Returns a handle for the other calls, or NULL with `error` set.
void *todex_camera_open(const char *device_id, uint32_t max_width, char *error, size_t error_len) {
    @autoreleasepool {
        if ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo] !=
            AVAuthorizationStatusAuthorized) {
            write_error(error, error_len, @"camera access is not granted to todex-agentd");
            return NULL;
        }
        NSString *wanted = [NSString stringWithUTF8String:device_id];
        AVCaptureDevice *device = wanted == nil ? nil : [AVCaptureDevice deviceWithUniqueID:wanted];
        if (device == nil) {
            write_error(error, error_len, @"the camera is not connected");
            return NULL;
        }
        NSError *failure = nil;
        AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:device error:&failure];
        if (input == nil) {
            write_error(error, error_len, failure.localizedDescription ?: @"cannot open the camera");
            return NULL;
        }
        AVCaptureSession *session = [AVCaptureSession new];
        if (![session canAddInput:input]) {
            write_error(error, error_len, @"the camera cannot be used now (in use by another app?)");
            return NULL;
        }
        [session addInput:input];

        CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(device.activeFormat.formatDescription);
        int32_t width = size.width > 0 ? size.width : 1280;
        int32_t height = size.height > 0 ? size.height : 720;
        if (max_width > 0 && (uint32_t)width > max_width) {
            height = (int32_t)((int64_t)height * max_width / width);
            width = (int32_t)max_width;
        }
        width &= ~1;
        height &= ~1;
        AVCaptureVideoDataOutput *output = [AVCaptureVideoDataOutput new];
        output.alwaysDiscardsLateVideoFrames = YES;
        output.videoSettings = @{
            (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_32BGRA),
            (id)kCVPixelBufferWidthKey : @(width),
            (id)kCVPixelBufferHeightKey : @(height),
        };
        TodexCamera *camera = [TodexCamera new];
        camera.sink = [TodexCameraSink new];
        camera.queue = dispatch_queue_create("todex.camera.frames", DISPATCH_QUEUE_SERIAL);
        [output setSampleBufferDelegate:camera.sink queue:camera.queue];
        if (![session canAddOutput:output]) {
            write_error(error, error_len, @"the camera cannot deliver frames");
            return NULL;
        }
        [session addOutput:output];
        camera.session = session;
        [session startRunning];
        if (!session.isRunning) {
            write_error(error, error_len, @"the camera did not start (in use by another app?)");
            return NULL;
        }
        return (void *)CFBridgingRetain(camera);
    }
}

// Copies the newest frame when it is newer than `*seq`: returns 1 and sets
// `*seq`, `*pixels` (BGRA, width * 4 bytes per row; free with
// `todex_camera_free`), `*width` and `*height`. Returns 0 when there is no
// newer frame, -1 when copying failed.
int32_t todex_camera_copy_frame(void *handle, uint64_t *seq, uint8_t **pixels, uint32_t *width,
                                uint32_t *height) {
    TodexCamera *camera = (__bridge TodexCamera *)handle;
    uint64_t latest_seq = 0;
    CVPixelBufferRef frame = [camera.sink copyLatest:&latest_seq];
    if (frame == NULL || latest_seq == *seq) {
        if (frame != NULL) {
            CVPixelBufferRelease(frame);
        }
        return 0;
    }
    int32_t result = -1;
    if (CVPixelBufferLockBaseAddress(frame, kCVPixelBufferLock_ReadOnly) == kCVReturnSuccess) {
        size_t w = CVPixelBufferGetWidth(frame);
        size_t h = CVPixelBufferGetHeight(frame);
        size_t stride = CVPixelBufferGetBytesPerRow(frame);
        const uint8_t *base = CVPixelBufferGetBaseAddress(frame);
        uint8_t *copy = base == NULL ? NULL : malloc(w * h * 4);
        if (copy != NULL) {
            for (size_t row = 0; row < h; row++) {
                memcpy(copy + row * w * 4, base + row * stride, w * 4);
            }
            *pixels = copy;
            *width = (uint32_t)w;
            *height = (uint32_t)h;
            *seq = latest_seq;
            result = 1;
        }
        CVPixelBufferUnlockBaseAddress(frame, kCVPixelBufferLock_ReadOnly);
    }
    CVPixelBufferRelease(frame);
    return result;
}

void todex_camera_close(void *handle) {
    @autoreleasepool {
        TodexCamera *camera = (TodexCamera *)CFBridgingRelease(handle);
        [camera.session stopRunning];
    }
}

void todex_camera_free(void *memory) {
    free(memory);
}
