// Computer Use screenshots through ScreenCaptureKit, beyond the display
// crop xa11y offers:
//
// - one window by its window-server number, drawn on its own (what covers
//   it is not in the picture), also when it is in another desktop Space;
// - a rectangle of the display that contains it (xa11y always captures the
//   first display), optionally with only one process's windows, so an app's
//   menus and sheets show but no other app does.
//
// Every entry point runs inside @try/@catch so an Objective-C exception
// never unwinds into Rust, and waits for ScreenCaptureKit at most
// `timeoutSeconds`. Pixels come back as RGBA8, row-major, owned by the
// caller (free with todex_capture_free).

#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

enum {
    TODEX_CAPTURE_OK = 0,
    TODEX_CAPTURE_NO_CONTENT = -1,
    TODEX_CAPTURE_NOT_FOUND = -2,
    TODEX_CAPTURE_FAILED = -3,
    TODEX_CAPTURE_NO_MEMORY = -4,
    TODEX_CAPTURE_BAD_SIZE = -5,
    TODEX_CAPTURE_TIMEOUT = -6,
    TODEX_CAPTURE_EXCEPTION = -9999,
};

// Waits for `semaphore`; whether it was signalled in time.
static BOOL todex_wait(dispatch_semaphore_t semaphore, double timeoutSeconds) {
    dispatch_time_t deadline =
        dispatch_time(DISPATCH_TIME_NOW, (int64_t)(timeoutSeconds * NSEC_PER_SEC));
    return dispatch_semaphore_wait(semaphore, deadline) == 0;
}

// Shareable content including windows not on screen (other Spaces).
static SCShareableContent *todex_content(double timeoutSeconds, int *status) {
    __block SCShareableContent *content = nil;
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    [SCShareableContent getShareableContentExcludingDesktopWindows:YES
                                               onScreenWindowsOnly:NO
                                                 completionHandler:^(SCShareableContent *c, NSError *e) {
        (void)e;
        content = c;
        dispatch_semaphore_signal(done);
    }];
    if (!todex_wait(done, timeoutSeconds)) {
        *status = TODEX_CAPTURE_TIMEOUT;
        return nil;
    }
    if (content == nil) {
        *status = TODEX_CAPTURE_NO_CONTENT;
    }
    return content;
}

// One image of `filter`; the result is retained (the caller releases it).
// A completion arriving after the timeout releases its own image.
static CGImageRef todex_shoot(SCContentFilter *filter, SCStreamConfiguration *config,
                              double timeoutSeconds, int *status) {
    __block CGImageRef image = NULL;
    __block BOOL abandoned = NO;
    NSObject *lock = [NSObject new];
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    [SCScreenshotManager captureImageWithFilter:filter
                                  configuration:config
                              completionHandler:^(CGImageRef captured, NSError *e) {
        (void)e;
        @synchronized(lock) {
            if (!abandoned && captured != NULL) {
                image = CGImageRetain(captured);
            }
        }
        dispatch_semaphore_signal(done);
    }];
    if (!todex_wait(done, timeoutSeconds)) {
        @synchronized(lock) {
            abandoned = YES;
            if (image != NULL) {
                CGImageRelease(image);
                image = NULL;
            }
        }
        *status = TODEX_CAPTURE_TIMEOUT;
        return NULL;
    }
    if (image == NULL) {
        *status = TODEX_CAPTURE_FAILED;
    }
    return image;
}

// Copies `image` into a new RGBA8 buffer.
static int todex_pixels(CGImageRef image, uint8_t **outPixels, uint32_t *outWidth,
                        uint32_t *outHeight) {
    size_t width = CGImageGetWidth(image);
    size_t height = CGImageGetHeight(image);
    if (width == 0 || height == 0) {
        return TODEX_CAPTURE_BAD_SIZE;
    }
    size_t rowBytes = width * 4;
    uint8_t *buffer = (uint8_t *)calloc(rowBytes * height, 1);
    if (buffer == NULL) {
        return TODEX_CAPTURE_NO_MEMORY;
    }
    CGColorSpaceRef space = CGColorSpaceCreateDeviceRGB();
    CGContextRef context = CGBitmapContextCreate(
        buffer, width, height, 8, rowBytes, space,
        kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
    CGColorSpaceRelease(space);
    if (context == NULL) {
        free(buffer);
        return TODEX_CAPTURE_NO_MEMORY;
    }
    CGContextDrawImage(context, CGRectMake(0, 0, (CGFloat)width, (CGFloat)height), image);
    CGContextRelease(context);
    *outPixels = buffer;
    *outWidth = (uint32_t)width;
    *outHeight = (uint32_t)height;
    return TODEX_CAPTURE_OK;
}

static void todex_reset(uint8_t **outPixels, uint32_t *outWidth, uint32_t *outHeight) {
    *outPixels = NULL;
    *outWidth = 0;
    *outHeight = 0;
}

// The window numbered `windowNumber`, on its own, at the display's pixel
// density. `TODEX_CAPTURE_NOT_FOUND` when ScreenCaptureKit does not list it.
int todex_capture_window(uint32_t windowNumber, double timeoutSeconds, uint8_t **outPixels,
                         uint32_t *outWidth, uint32_t *outHeight) {
    @try {
        todex_reset(outPixels, outWidth, outHeight);
        int status = TODEX_CAPTURE_OK;
        SCShareableContent *content = todex_content(timeoutSeconds, &status);
        if (content == nil) {
            return status;
        }
        SCWindow *window = nil;
        for (SCWindow *candidate in content.windows) {
            if (candidate.windowID == windowNumber) {
                window = candidate;
                break;
            }
        }
        if (window == nil) {
            return TODEX_CAPTURE_NOT_FOUND;
        }
        SCContentFilter *filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:window];
        CGRect frame = filter.contentRect;
        double scale = filter.pointPixelScale > 0 ? filter.pointPixelScale : 1.0;
        size_t width = (size_t)(frame.size.width * scale + 0.5);
        size_t height = (size_t)(frame.size.height * scale + 0.5);
        if (width == 0 || height == 0) {
            return TODEX_CAPTURE_BAD_SIZE;
        }
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        config.width = width;
        config.height = height;
        config.pixelFormat = 'BGRA';
        config.colorSpaceName = kCGColorSpaceSRGB;
        config.showsCursor = NO;
        config.ignoreShadowsSingleWindow = YES;
        CGImageRef image = todex_shoot(filter, config, timeoutSeconds, &status);
        if (image == NULL) {
            return status;
        }
        status = todex_pixels(image, outPixels, outWidth, outHeight);
        CGImageRelease(image);
        return status;
    } @catch (NSException *e) {
        todex_reset(outPixels, outWidth, outHeight);
        return TODEX_CAPTURE_EXCEPTION;
    }
}

// The rectangle (global points, top-left origin) of the display that
// contains its centre. `onlyPid` > 0 keeps that process's on-screen windows
// alone in the picture; 0 captures everything shown there.
int todex_capture_region(double x, double y, double width, double height, int32_t onlyPid,
                         double timeoutSeconds, uint8_t **outPixels, uint32_t *outWidth,
                         uint32_t *outHeight) {
    @try {
        todex_reset(outPixels, outWidth, outHeight);
        if (width <= 0.0 || height <= 0.0) {
            return TODEX_CAPTURE_BAD_SIZE;
        }
        int status = TODEX_CAPTURE_OK;
        SCShareableContent *content = todex_content(timeoutSeconds, &status);
        if (content == nil) {
            return status;
        }
        CGPoint centre = CGPointMake(x + width / 2.0, y + height / 2.0);
        SCDisplay *display = nil;
        for (SCDisplay *candidate in content.displays) {
            if (CGRectContainsPoint(candidate.frame, centre)) {
                display = candidate;
                break;
            }
        }
        if (display == nil) {
            display = content.displays.firstObject;
        }
        if (display == nil) {
            return TODEX_CAPTURE_NO_CONTENT;
        }
        SCContentFilter *filter;
        if (onlyPid > 0) {
            NSMutableArray<SCWindow *> *windows = [NSMutableArray array];
            for (SCWindow *window in content.windows) {
                if (window.onScreen && window.owningApplication.processID == onlyPid) {
                    [windows addObject:window];
                }
            }
            filter = [[SCContentFilter alloc] initWithDisplay:display includingWindows:windows];
        } else {
            filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        }
        double scale = filter.pointPixelScale > 0 ? filter.pointPixelScale : 1.0;
        size_t pixelWidth = (size_t)(width * scale + 0.5);
        size_t pixelHeight = (size_t)(height * scale + 0.5);
        if (pixelWidth == 0 || pixelHeight == 0) {
            return TODEX_CAPTURE_BAD_SIZE;
        }
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        config.width = pixelWidth;
        config.height = pixelHeight;
        config.pixelFormat = 'BGRA';
        config.colorSpaceName = kCGColorSpaceSRGB;
        config.showsCursor = NO;
        // ScreenCaptureKit's source rectangle is in the display's own points.
        config.sourceRect = CGRectMake(x - display.frame.origin.x, y - display.frame.origin.y,
                                       width, height);
        config.destinationRect = CGRectMake(0, 0, (CGFloat)pixelWidth, (CGFloat)pixelHeight);
        CGImageRef image = todex_shoot(filter, config, timeoutSeconds, &status);
        if (image == NULL) {
            return status;
        }
        status = todex_pixels(image, outPixels, outWidth, outHeight);
        CGImageRelease(image);
        return status;
    } @catch (NSException *e) {
        todex_reset(outPixels, outWidth, outHeight);
        return TODEX_CAPTURE_EXCEPTION;
    }
}

void todex_capture_free(uint8_t *pixels) {
    free(pixels);
}
