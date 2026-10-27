/*
 * RML mac fix: the Mac-side helper the launcher loads into the game's Wine
 * process (DYLD_INSERT_LIBRARIES). It does two things Windows code cannot:
 *
 *  - full screen (RML_FULLSCREEN=1): once the game window appears it is put
 *    into macOS full screen on its own Space, like the green window button;
 *    afterwards the player can leave full screen as usual.
 *  - cursor keeper (on unless RML_CURSOR_FIX=0): after clicks macOS shows its
 *    arrow, while Wine believes the game's cursor is still shown; the keeper
 *    re-applies Wine's cursor after every click.
 *  - display profile cache (on unless RML_DISPLAY_CACHE=0): DXMT 0.80 reads
 *    the display's color profile and screen description for every frame and
 *    never frees them (about 55 MB per minute); the cache hands out the same
 *    objects for a few seconds instead.
 *
 * Wine creates its own NSApplication; this library never creates one, it only
 * reads Wine's application and its cursor controller (Wine is open source,
 * see dlls/winemac.drv/cocoa_app.m). Only the game's process is touched
 * (RML_GAME_PROCESS).
 *
 * Part of RML – Runehaven Mac Launcher (unofficial). GPL-3.0-or-later, see LICENSE and NOTICE.md.
 */
#import <Foundation/Foundation.h>
#import <ColorSync/ColorSync.h>
#include <CoreGraphics/CGDirectDisplay.h>
#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>
#include <os/lock.h>
#include <pthread.h>
#include <objc/message.h>
#include <objc/runtime.h>
#include <stdio.h>
#include <string.h>
#include <strings.h>
#include <unistd.h>
#include <stdlib.h>

/* AppKit constants, so that AppKit need not be linked. */
#define FULLSCREEN_PRIMARY (1UL << 7)  /* NSWindowCollectionBehaviorFullScreenPrimary */
#define STYLE_FULLSCREEN (1UL << 14)   /* NSWindowStyleMaskFullScreen */

static const double POLL_SECONDS = 0.25;
static const double GIVE_UP_SECONDS = 180; /* the game may take a while to show */
static int ticks;
static int ready_ticks;                    /* consecutive checks the window qualified */
static volatile int finished;

#ifdef RML_TEST_CONTROL
void rml_test_control_start(void); /* testcontrol.m, development builds only */
#endif

static void rml_log(const char *msg) {
    fprintf(stderr, "[rml-macfix] %d: %s\n", getpid(), msg);
}

/* Wine's NSApp, or nil while Wine has not created it yet. */
static id wine_app(void) {
    id *app = (id *)dlsym(RTLD_DEFAULT, "NSApp");
    return app ? *app : nil;
}

static id send_id(id obj, const char *sel) {
    return ((id(*)(id, SEL))objc_msgSend)(obj, sel_registerName(sel));
}

static NSUInteger send_uint(id obj, const char *sel) {
    return ((NSUInteger(*)(id, SEL))objc_msgSend)(obj, sel_registerName(sel));
}

static BOOL send_bool(id obj, const char *sel) {
    return ((BOOL(*)(id, SEL))objc_msgSend)(obj, sel_registerName(sel));
}

/* Logs a state description only when it changed, to keep the log short. */
static char last_state[256];
static void log_state(const char *state) {
    if (strcmp(state, last_state) == 0) return;
    strncpy(last_state, state, sizeof last_state - 1);
    rml_log(state);
}

/* The window that should go full screen: Wine's largest visible window that
 * may enter full screen. Wine's app is not necessarily active yet when the
 * game window appears, so the key window is not relied on. */
static id candidate_window(id app) {
    NSArray *windows = (NSArray *)send_id(app, "windows");
    id best = nil;
    double best_area = 0;
    int visible = 0, capable = 0;
    for (id win in windows) {
        if (!send_bool(win, "isVisible")) continue;
        visible++;
        if (send_uint(win, "styleMask") & STYLE_FULLSCREEN) {
            log_state("a window is already in full screen");
            return nil;
        }
        if (!(send_uint(win, "collectionBehavior") & FULLSCREEN_PRIMARY)) continue;
        capable++;
        /* Through KVC, so the compiler picks the right call for a struct result. */
        NSRect frame = [(NSValue *)[win valueForKey:@"frame"] rectValue];
        double area = frame.size.width * frame.size.height;
        if (frame.size.width >= 320 && frame.size.height >= 200 && area > best_area) {
            best = win;
            best_area = area;
        }
    }
    char state[128];
    snprintf(state, sizeof state, "%lu windows, %d visible, %d can go full screen",
             (unsigned long)[windows count], visible, capable);
    log_state(state);
    return best;
}

static int want_fullscreen, want_cursor;
static int fullscreen_done, keeper_installed;

/*
 * Cursor keeper. Runehaven sets its cursor once; macOS puts its own arrow
 * back after clicks into the window, and Wine believes its cursor is still
 * shown, so the arrow stays (or flashes). After every click the cursor Wine
 * wants is applied again, unless the game hid it.
 */
static void reapply_cursor(void) {
    Class cls = objc_getClass("WineApplicationController");
    if (!cls) return;
    id controller = send_id((id)cls, "sharedController");
    if (!controller) return;
    @try {
        if ([[controller valueForKey:@"clientWantsCursorHidden"] boolValue]) return;
        id cursor = [controller valueForKey:@"cursor"];
        if (cursor) ((void (*)(id, SEL))objc_msgSend)(cursor, sel_registerName("set"));
    } @catch (id e) {
        (void)e;
    }
}

static void install_cursor_keeper(void) {
    Class event_cls = objc_getClass("NSEvent");
    if (!event_cls) return;
    /* left/right/other mouse down and up */
    unsigned long long mask = (1ULL << 1) | (1ULL << 2) | (1ULL << 3) | (1ULL << 4) |
                              (1ULL << 25) | (1ULL << 26);
    id (^handler)(id) = ^id(id event) {
        CFRunLoopRef main_loop = CFRunLoopGetMain();
        CFRunLoopPerformBlock(main_loop, kCFRunLoopCommonModes, ^{
            reapply_cursor();
        });
        CFRunLoopWakeUp(main_loop);
        return event;
    };
    id monitor = ((id(*)(id, SEL, unsigned long long, id))objc_msgSend)(
        (id)event_cls, sel_registerName("addLocalMonitorForEventsMatchingMask:handler:"), mask,
        handler);
    keeper_installed = 1;
    rml_log(monitor ? "cursor keeper active" : "cursor keeper could not be installed");
}

static void try_fullscreen(id app) {
    if (ticks * POLL_SECONDS > GIVE_UP_SECONDS) {
        fullscreen_done = 1;
        rml_log("no game window that can go full screen appeared; giving up");
        return;
    }
    id win = candidate_window(app);
    if (!win) {
        ready_ticks = 0;
        return;
    }
    /* Give the game a moment after its window appeared before switching. */
    if (++ready_ticks < 3) return;
    ((void (*)(id, SEL, BOOL))objc_msgSend)(app, sel_registerName("activateIgnoringOtherApps:"), YES);
    ((void (*)(id, SEL, id))objc_msgSend)(win, sel_registerName("makeKeyAndOrderFront:"), nil);
    ((void (*)(id, SEL, id))objc_msgSend)(win, sel_registerName("toggleFullScreen:"), nil);
    fullscreen_done = 1;
    rml_log("game window switched to macOS full screen");
}

static void tick(void) {
    ticks++;
    id app = wine_app();
    if (!app) {
        log_state("waiting for Wine's application");
        return;
    }
    if (want_cursor && !keeper_installed) install_cursor_keeper();
    if (want_fullscreen && !fullscreen_done) try_fullscreen(app);
    if ((!want_cursor || keeper_installed) && (!want_fullscreen || fullscreen_done)) finished = 1;
}

static void *poll_thread(void *arg) {
    (void)arg;
    char msg[160];
    snprintf(msg, sizeof msg, "watching for the game window (%s)", getprogname());
    rml_log(msg);
    while (!finished) {
        usleep((useconds_t)(POLL_SECONDS * 1e6));
        CFRunLoopRef main_loop = CFRunLoopGetMain();
        CFRunLoopPerformBlock(main_loop, kCFRunLoopCommonModes, ^{
            if (!finished) tick();
        });
        CFRunLoopWakeUp(main_loop);
    }
    return NULL;
}

static int env_on(const char *name, int fallback) {
    const char *v = getenv(name);
    if (!v || !*v) return fallback;
    return strcmp(v, "0") != 0;
}

static void start_watching(void) {
    static int started;
    if (started) return;
    /* Only the game's process (RML_GAME_PROCESS) is touched; Wine's helper
     * processes are left alone. */
    const char *only = getenv("RML_GAME_PROCESS");
    if (only && *only && strcasecmp(getprogname(), only) != 0) return;
#ifdef RML_TEST_CONTROL
    rml_test_control_start(); /* development builds only, see testcontrol.m */
#endif
    want_fullscreen = env_on("RML_FULLSCREEN", 0);
    want_cursor = env_on("RML_CURSOR_FIX", 1);
    if (!want_fullscreen && !want_cursor) return;
    started = 1;
    pthread_t thread;
    if (pthread_create(&thread, NULL, poll_thread, NULL) == 0) pthread_detach(thread);
}

/*
 * Display profile cache. DXMT's _WMTGetDisplayDescription (winemetal.so,
 * src/winemetal/unix/winemetal_unix.c in 0.80) runs for every frame: it
 * creates the display's ColorSync profile, copies four tags from it and looks
 * up the NSScreen via -deviceDescription, frees none of it and has no
 * autorelease pool of its own. Here the profile, its tags and each screen's
 * description are kept for CACHE_SECONDS and handed out again, retained as
 * the callers expect; the leak shrinks to one profile per refresh.
 */
static const double CACHE_SECONDS = 5;
enum { MAX_DISPLAYS = 8, MAX_TAGS = 32 };

static os_unfair_lock cache_lock = OS_UNFAIR_LOCK_INIT;
static struct {
    CGDirectDisplayID id;
    ColorSyncProfileRef profile;
    double created;
} profiles[MAX_DISPLAYS];
static struct {
    ColorSyncProfileRef profile;
    CFStringRef tag;
    CFDataRef data;
} tags[MAX_TAGS];

static double now_seconds(void) {
    return CFAbsoluteTimeGetCurrent();
}

/* Whether the cache works in this process: only in the game's process, and
 * RML_DISPLAY_CACHE=0 turns it off. */
static int cache_enabled(void) {
    static int enabled = -1;
    if (enabled < 0) {
        const char *only = getenv("RML_GAME_PROCESS");
        int game = !only || !*only || strcasecmp(getprogname(), only) == 0;
        enabled = game && env_on("RML_DISPLAY_CACHE", 1);
    }
    return enabled;
}

static void drop_tags_of(ColorSyncProfileRef profile) {
    for (int i = 0; i < MAX_TAGS; i++) {
        if (tags[i].profile != profile) continue;
        CFRelease(tags[i].tag);
        CFRelease(tags[i].data);
        tags[i].profile = NULL;
    }
}

static int is_cached_profile(ColorSyncProfileRef profile) {
    for (int i = 0; i < MAX_DISPLAYS; i++)
        if (profiles[i].profile && profiles[i].profile == profile) return 1;
    return 0;
}

/* -[NSScreen deviceDescription], cached per screen. */
static IMP original_device_description;
static const char description_key;
static const char description_time_key;

static id rml_deviceDescription(id self, SEL cmd) {
    os_unfair_lock_lock(&cache_lock);
    id cached = objc_getAssociatedObject(self, &description_key);
    NSNumber *created = objc_getAssociatedObject(self, &description_time_key);
    if (cached && now_seconds() - [created doubleValue] < CACHE_SECONDS) {
        [cached retain];
        os_unfair_lock_unlock(&cache_lock);
        return [cached autorelease];
    }
    os_unfair_lock_unlock(&cache_lock);
    id fresh = ((id(*)(id, SEL))original_device_description)(self, cmd);
    if (!fresh) return fresh;
    os_unfair_lock_lock(&cache_lock);
    objc_setAssociatedObject(self, &description_key, fresh, OBJC_ASSOCIATION_RETAIN);
    objc_setAssociatedObject(self, &description_time_key, @(now_seconds()), OBJC_ASSOCIATION_RETAIN);
    os_unfair_lock_unlock(&cache_lock);
    return fresh;
}

/* Installed on DXMT's first profile request; AppKit is loaded by then. */
static void install_description_cache(void) {
    static int installed;
    if (installed) return;
    Class screen_cls = objc_getClass("NSScreen");
    if (!screen_cls) return;
    installed = 1;
    Method m = class_getInstanceMethod(screen_cls, sel_registerName("deviceDescription"));
    if (!m) {
        rml_log("display profile cache: NSScreen deviceDescription not found");
        return;
    }
    original_device_description = method_setImplementation(m, (IMP)rml_deviceDescription);
    rml_log("display profile cache active");
}

static ColorSyncProfileRef rml_ColorSyncProfileCreateWithDisplayID(CGDirectDisplayID display_id) {
    if (!cache_enabled()) return ColorSyncProfileCreateWithDisplayID(display_id);
    os_unfair_lock_lock(&cache_lock);
    install_description_cache();
    for (int i = 0; i < MAX_DISPLAYS; i++) {
        if (profiles[i].profile && profiles[i].id == display_id &&
            now_seconds() - profiles[i].created < CACHE_SECONDS) {
            ColorSyncProfileRef profile = (ColorSyncProfileRef)CFRetain(profiles[i].profile);
            os_unfair_lock_unlock(&cache_lock);
            return profile;
        }
    }
    os_unfair_lock_unlock(&cache_lock);

    /* Outside the lock: ColorSync may call back into the frameworks. */
    ColorSyncProfileRef profile = ColorSyncProfileCreateWithDisplayID(display_id);
    if (!profile) return profile;
    os_unfair_lock_lock(&cache_lock);
    int slot = -1, oldest = 0;
    for (int i = 0; i < MAX_DISPLAYS; i++) {
        if (profiles[i].profile && profiles[i].id == display_id) {
            slot = i;
            break;
        }
        if (profiles[i].created < profiles[oldest].created) oldest = i;
    }
    if (slot < 0) slot = oldest;
    if (profiles[slot].profile) {
        drop_tags_of(profiles[slot].profile);
        CFRelease(profiles[slot].profile);
    }
    profiles[slot].id = display_id;
    profiles[slot].profile = (ColorSyncProfileRef)CFRetain(profile);
    profiles[slot].created = now_seconds();
    os_unfair_lock_unlock(&cache_lock);
    return profile;
}

static CFDataRef rml_ColorSyncProfileCopyTag(ColorSyncProfileRef profile, CFStringRef tag) {
    if (!cache_enabled() || !profile || !tag) return ColorSyncProfileCopyTag(profile, tag);
    os_unfair_lock_lock(&cache_lock);
    int cached = is_cached_profile(profile);
    for (int i = 0; cached && i < MAX_TAGS; i++) {
        if (tags[i].profile == profile && CFEqual(tags[i].tag, tag)) {
            CFDataRef data = (CFDataRef)CFRetain(tags[i].data);
            os_unfair_lock_unlock(&cache_lock);
            return data;
        }
    }
    os_unfair_lock_unlock(&cache_lock);

    CFDataRef data = ColorSyncProfileCopyTag(profile, tag);
    if (!data || !cached) return data;
    os_unfair_lock_lock(&cache_lock);
    /* Store only while the profile is still cached and the tag is not yet. */
    int free_slot = -1;
    if (is_cached_profile(profile)) {
        for (int i = 0; i < MAX_TAGS; i++) {
            if (!tags[i].profile) {
                if (free_slot < 0) free_slot = i;
            } else if (tags[i].profile == profile && CFEqual(tags[i].tag, tag)) {
                free_slot = -1;
                break;
            }
        }
    }
    if (free_slot >= 0) {
        tags[free_slot].profile = profile;
        tags[free_slot].tag = (CFStringRef)CFRetain(tag);
        tags[free_slot].data = (CFDataRef)CFRetain(data);
    }
    os_unfair_lock_unlock(&cache_lock);
    return data;
}

/*
 * Wine starts the game in a second step (through its "preloader"), where
 * library constructors may not run. But Wine then parks the main thread in
 * CFRunLoopRun() for the Mac frameworks, so this library replaces that call
 * (dyld "interposing") and starts watching from there.
 */
static void rml_CFRunLoopRun(void) {
    if (pthread_main_np()) start_watching();
    CFRunLoopRun();
}

__attribute__((used)) static const struct {
    const void *replacement;
    const void *replacee;
} interposers[] __attribute__((section("__DATA,__interpose"))) = {
    {(const void *)rml_CFRunLoopRun, (const void *)CFRunLoopRun},
    {(const void *)rml_ColorSyncProfileCreateWithDisplayID, (const void *)ColorSyncProfileCreateWithDisplayID},
    {(const void *)rml_ColorSyncProfileCopyTag, (const void *)ColorSyncProfileCopyTag},
};

