/*
 * RML test control – DEVELOPMENT ONLY, REMOVE BEFORE BUILDING THE APP.
 *
 * Lets a developer (or an assistant on the developer's Mac) drive the game
 * during tests: clicks, key presses and screenshots of the game window. It is
 * only active with RML_TEST_CONTROL=1 and listens on a Unix socket that only
 * the current user can open (RML_TEST_SOCKET, default under /tmp with the
 * user's id). Events go into Wine's own event queue, so no other app is
 * touched.
 *
 * Protocol: one command per line, one reply line ("ok ..." or "err ...").
 *   state                      window size in points, full screen, active
 *   click <fx> <fy> [right]    click at a fraction (0..1) of the window
 *                              content, measured from the top left
 *   move <fx> <fy>             move the mouse there
 *   key <name> [hold seconds]  press a key (Return, Escape, Tab, Space,
 *                              Up/Down/Left/Right, a-z, 0-9, Shift, Ctrl)
 *   shot <path> [scale]        save a PNG of the game window (scale 0.1..1)
 *
 * Example: echo "click 0.5 0.6" | nc -U /tmp/rml-test-501.sock
 *
 * Part of RML – Runehaven Mac Launcher (unofficial). MIT License.
 */
#import <Foundation/Foundation.h>
#include <CoreGraphics/CoreGraphics.h>
#include <ImageIO/ImageIO.h>
#include <dispatch/dispatch.h>
#include <dlfcn.h>
#include <objc/message.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <strings.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

void rml_test_control_start(void);

/* AppKit constants, so that AppKit need not be linked. */
enum {
    EV_LEFT_DOWN = 1, EV_LEFT_UP = 2, EV_RIGHT_DOWN = 3, EV_RIGHT_UP = 4,
    EV_MOUSE_MOVED = 5, EV_KEY_DOWN = 10, EV_KEY_UP = 11, EV_FLAGS_CHANGED = 12,
};
#define STYLE_FULLSCREEN (1UL << 14)
#define FLAG_SHIFT (1UL << 17)
#define FLAG_CONTROL (1UL << 18)

static void tc_log(const char *msg) {
    fprintf(stderr, "[rml-macfix] %d: test control: %s\n", getpid(), msg);
}

static id tc_app(void) {
    id *app = (id *)dlsym(RTLD_DEFAULT, "NSApp");
    return app ? *app : nil;
}

static id tc_send(id obj, const char *sel) {
    return ((id(*)(id, SEL))objc_msgSend)(obj, sel_registerName(sel));
}

/* The game window: Wine's largest visible window. */
static id game_window(void) {
    id app = tc_app();
    if (!app) return nil;
    id best = nil;
    double best_area = 0;
    for (id win in (NSArray *)tc_send(app, "windows")) {
        if (!((BOOL(*)(id, SEL))objc_msgSend)(win, sel_registerName("isVisible"))) continue;
        NSRect f = [(NSValue *)[win valueForKey:@"frame"] rectValue];
        if (f.size.width * f.size.height > best_area) {
            best = win;
            best_area = f.size.width * f.size.height;
        }
    }
    return best;
}

static NSRect content_rect(id win) {
    id view = tc_send(win, "contentView");
    return [(NSValue *)[view valueForKey:@"frame"] rectValue];
}

/* fx/fy from the top left of the content -> window coordinates (bottom left). */
static NSPoint window_point(id win, double fx, double fy) {
    NSRect c = content_rect(win);
    return NSMakePoint(c.origin.x + fx * c.size.width, c.origin.y + (1.0 - fy) * c.size.height);
}

static void post(id event) {
    if (!event) return;
    ((void (*)(id, SEL, id, BOOL))objc_msgSend)(tc_app(), sel_registerName("postEvent:atStart:"), event, NO);
}

static id mouse_event(unsigned long type, id win, NSPoint p) {
    Class cls = objc_getClass("NSEvent");
    long win_num = ((long (*)(id, SEL))objc_msgSend)(win, sel_registerName("windowNumber"));
    double ts = [[NSProcessInfo processInfo] systemUptime];
    return ((id(*)(id, SEL, unsigned long, NSPoint, unsigned long, double, long, id, long, long, float))
                objc_msgSend)((id)cls,
                              sel_registerName("mouseEventWithType:location:modifierFlags:timestamp:"
                                               "windowNumber:context:eventNumber:clickCount:pressure:"),
                              type, p, 0, ts, win_num, nil, 0, 1, type == EV_MOUSE_MOVED ? 0.f : 1.f);
}

static const struct { const char *name; unsigned short code; const char *chars; } keys[] = {
    {"return", 36, "\r"}, {"enter", 36, "\r"}, {"escape", 53, "\x1b"}, {"esc", 53, "\x1b"},
    {"tab", 48, "\t"}, {"space", 49, " "}, {"backspace", 51, "\x7f"},
    {"left", 123, ""}, {"right", 124, ""}, {"down", 125, ""}, {"up", 126, ""},
    {"shift", 56, ""}, {"ctrl", 59, ""},
    {"a", 0, "a"}, {"s", 1, "s"}, {"d", 2, "d"}, {"f", 3, "f"}, {"h", 4, "h"}, {"g", 5, "g"},
    {"z", 6, "z"}, {"x", 7, "x"}, {"c", 8, "c"}, {"v", 9, "v"}, {"b", 11, "b"}, {"q", 12, "q"},
    {"w", 13, "w"}, {"e", 14, "e"}, {"r", 15, "r"}, {"y", 16, "y"}, {"t", 17, "t"},
    {"1", 18, "1"}, {"2", 19, "2"}, {"3", 20, "3"}, {"4", 21, "4"}, {"6", 22, "6"},
    {"5", 23, "5"}, {"9", 25, "9"}, {"7", 26, "7"}, {"8", 28, "8"}, {"0", 29, "0"},
    {"o", 31, "o"}, {"u", 32, "u"}, {"i", 34, "i"}, {"p", 35, "p"}, {"l", 37, "l"},
    {"j", 38, "j"}, {"k", 40, "k"}, {"n", 45, "n"}, {"m", 46, "m"},
};

static id key_event(unsigned long type, id win, unsigned short code, const char *chars, unsigned long flags) {
    Class cls = objc_getClass("NSEvent");
    long win_num = ((long (*)(id, SEL))objc_msgSend)(win, sel_registerName("windowNumber"));
    double ts = [[NSProcessInfo processInfo] systemUptime];
    NSString *s = [NSString stringWithUTF8String:chars];
    return ((id(*)(id, SEL, unsigned long, NSPoint, unsigned long, double, long, id, id, id, BOOL,
                   unsigned short))objc_msgSend)(
        (id)cls,
        sel_registerName("keyEventWithType:location:modifierFlags:timestamp:windowNumber:context:"
                         "characters:charactersIgnoringModifiers:isARepeat:keyCode:"),
        type, NSZeroPoint, flags, ts, win_num, nil, s, s, NO, code);
}

static void after(double seconds, dispatch_block_t block) {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(seconds * NSEC_PER_SEC)), dispatch_get_main_queue(),
                   ^{
                       CFRunLoopPerformBlock(CFRunLoopGetMain(), kCFRunLoopCommonModes, block);
                       CFRunLoopWakeUp(CFRunLoopGetMain());
                   });
}

static NSString *do_shot(id win, const char *path, double scale) {
    CGWindowID wid = (CGWindowID)((long (*)(id, SEL))objc_msgSend)(win, sel_registerName("windowNumber"));
    /* Deprecated in newer SDKs, but still the simplest capture of one window. */
    typedef CGImageRef (*capture_fn)(CGRect, uint32_t, CGWindowID, uint32_t);
    capture_fn capture = (capture_fn)dlsym(RTLD_DEFAULT, "CGWindowListCreateImage");
    if (!capture) return @"err CGWindowListCreateImage not available";
    CGImageRef img = capture(CGRectNull, 1 << 3 /* IncludingWindow */, wid,
                             1 << 0 /* BoundsIgnoreFraming */);
    if (!img) return @"err capture failed (screen recording permission?)";
    size_t w = CGImageGetWidth(img), h = CGImageGetHeight(img);
    if (scale > 0 && scale < 1) {
        size_t sw = (size_t)(w * scale), sh = (size_t)(h * scale);
        CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
        CGContextRef ctx = CGBitmapContextCreate(NULL, sw, sh, 8, 0, cs, (CGBitmapInfo)kCGImageAlphaPremultipliedLast);
        CGColorSpaceRelease(cs);
        CGContextSetInterpolationQuality(ctx, kCGInterpolationHigh);
        CGContextDrawImage(ctx, CGRectMake(0, 0, sw, sh), img);
        CGImageRelease(img);
        img = CGBitmapContextCreateImage(ctx);
        CGContextRelease(ctx);
        w = sw;
        h = sh;
    }
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)path, strlen(path), false);
    CGImageDestinationRef dst = CGImageDestinationCreateWithURL(url, CFSTR("public.png"), 1, NULL);
    CFRelease(url);
    if (!dst) {
        CGImageRelease(img);
        return @"err cannot write file";
    }
    CGImageDestinationAddImage(dst, img, NULL);
    bool ok = CGImageDestinationFinalize(dst);
    CFRelease(dst);
    CGImageRelease(img);
    return ok ? [NSString stringWithFormat:@"ok %zux%zu %s", w, h, path] : @"err cannot write file";
}

/* Runs one command on the main thread and returns the reply. */
static NSString *run_command(NSString *line) {
    NSArray *parts = [[line stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]]
        componentsSeparatedByCharactersInSet:[NSCharacterSet whitespaceCharacterSet]];
    if ([parts count] == 0 || [parts[0] length] == 0) return @"err empty";
    NSString *cmd = [parts[0] lowercaseString];
    id win = game_window();
    if (!win) return @"err no game window yet";

    if ([cmd isEqualToString:@"state"]) {
        NSRect c = content_rect(win);
        unsigned long style = ((unsigned long (*)(id, SEL))objc_msgSend)(win, sel_registerName("styleMask"));
        BOOL active = ((BOOL(*)(id, SEL))objc_msgSend)(tc_app(), sel_registerName("isActive"));
        return [NSString stringWithFormat:@"ok size %.0fx%.0f fullscreen %d active %d", c.size.width,
                                          c.size.height, (style & STYLE_FULLSCREEN) != 0, active];
    }
    if ([cmd isEqualToString:@"click"] || [cmd isEqualToString:@"move"]) {
        if ([parts count] < 3) return @"err usage: click <fx> <fy> [right]";
        double fx = [parts[1] doubleValue], fy = [parts[2] doubleValue];
        if (fx < 0 || fx > 1 || fy < 0 || fy > 1) return @"err fx/fy must be 0..1";
        NSPoint p = window_point(win, fx, fy);
        post(mouse_event(EV_MOUSE_MOVED, win, p));
        if ([cmd isEqualToString:@"move"]) return @"ok";
        BOOL right = [parts count] > 3 && [[parts[3] lowercaseString] isEqualToString:@"right"];
        after(0.05, ^{ post(mouse_event(right ? EV_RIGHT_DOWN : EV_LEFT_DOWN, win, p)); });
        after(0.12, ^{ post(mouse_event(right ? EV_RIGHT_UP : EV_LEFT_UP, win, p)); });
        return @"ok";
    }
    if ([cmd isEqualToString:@"key"]) {
        if ([parts count] < 2) return @"err usage: key <name> [hold seconds]";
        const char *name = [[parts[1] lowercaseString] UTF8String];
        double hold = [parts count] > 2 ? [parts[2] doubleValue] : 0.08;
        for (size_t i = 0; i < sizeof keys / sizeof keys[0]; i++) {
            if (strcmp(keys[i].name, name) != 0) continue;
            unsigned short code = keys[i].code;
            const char *chars = keys[i].chars;
            if (code == 56 || code == 59) { /* modifiers arrive as flags changes */
                unsigned long flag = code == 56 ? FLAG_SHIFT : FLAG_CONTROL;
                post(key_event(EV_FLAGS_CHANGED, win, code, "", flag));
                after(hold, ^{ post(key_event(EV_FLAGS_CHANGED, win, code, "", 0)); });
            } else {
                post(key_event(EV_KEY_DOWN, win, code, chars, 0));
                after(hold, ^{ post(key_event(EV_KEY_UP, win, code, chars, 0)); });
            }
            return @"ok";
        }
        return @"err unknown key";
    }
    if ([cmd isEqualToString:@"shot"]) {
        if ([parts count] < 2) return @"err usage: shot <path> [scale]";
        double scale = [parts count] > 2 ? [parts[2] doubleValue] : 1.0;
        return do_shot(win, [parts[1] UTF8String], scale);
    }
    return @"err unknown command";
}

static NSString *run_on_main(NSString *line) {
    __block NSString *reply = nil;
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    CFRunLoopPerformBlock(CFRunLoopGetMain(), kCFRunLoopCommonModes, ^{
        @autoreleasepool {
            @try {
                reply = [run_command(line) copy];
            } @catch (id e) {
                reply = [[NSString stringWithFormat:@"err %@", e] copy];
            }
        }
        dispatch_semaphore_signal(done);
    });
    CFRunLoopWakeUp(CFRunLoopGetMain());
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC)) != 0)
        return @"err main thread busy";
    return [reply autorelease];
}

static void serve_client(int fd) {
    FILE *in = fdopen(fd, "r+");
    if (!in) {
        close(fd);
        return;
    }
    char buf[1024];
    while (fgets(buf, sizeof buf, in)) {
        @autoreleasepool {
            NSString *reply = run_on_main([NSString stringWithUTF8String:buf] ?: @"");
            fprintf(in, "%s\n", [reply UTF8String]);
            fflush(in);
        }
    }
    fclose(in);
}

static void *server_thread(void *arg) {
    int srv = (int)(intptr_t)arg;
    for (;;) {
        int fd = accept(srv, NULL, NULL);
        if (fd < 0) continue;
        serve_client(fd);
    }
    return NULL;
}

void rml_test_control_start(void) {
    const char *on = getenv("RML_TEST_CONTROL");
    if (!on || strcmp(on, "1") != 0) return;
    char path[104];
    const char *env_path = getenv("RML_TEST_SOCKET");
    if (env_path && *env_path)
        snprintf(path, sizeof path, "%s", env_path);
    else
        snprintf(path, sizeof path, "/tmp/rml-test-%d.sock", (int)getuid());
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    if (srv < 0) return;
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    snprintf(addr.sun_path, sizeof addr.sun_path, "%s", path);
    unlink(path);
    mode_t old = umask(0077);
    int bound = bind(srv, (struct sockaddr *)&addr, sizeof addr);
    umask(old);
    if (bound != 0 || listen(srv, 2) != 0) {
        close(srv);
        tc_log("could not open the socket");
        return;
    }
    chmod(path, 0600);
    pthread_t thread;
    if (pthread_create(&thread, NULL, server_thread, (void *)(intptr_t)srv) == 0) pthread_detach(thread);
    char msg[160];
    snprintf(msg, sizeof msg, "listening on %s (development only)", path);
    tc_log(msg);
}
