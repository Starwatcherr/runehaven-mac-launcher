/*
 * Runehaven Launcher input fix.
 *
 * Unity's Windows player turns on "mouse in pointer" mode (EnableMouseInPointer)
 * and then reads the mouse through WM_POINTER* messages. Wine 9 does not
 * implement that mode, so the game never sees mouse input. This DLL:
 *
 *   1. reports EnableMouseInPointer as successful and the mouse as a pointer
 *      of type PT_MOUSE,
 *   2. watches the normal mouse messages of the game's window thread and posts
 *      the matching WM_POINTER* message for each of them, carrying the full
 *      button state from the mouse message (so held buttons stay held),
 *   3. answers GetPointerInfo & co. with the state that belongs to the pointer
 *      message being processed,
 *   4. re-applies the game's cursor after the window was activated, because the
 *      macOS cursor otherwise stays until the mouse leaves and re-enters.
 *
 * Only UnityPlayer.dll's import table is changed; no system code is patched.
 * Written from the public Win32 documentation.
 *
 * Environment: RHL_INPUTFIX=0 disables everything except forwarding,
 *              RHL_INPUTFIX_LOG=1 writes %TEMP%\rhl-inputfix.log (any other
 *              value is used as the log file's path),
 *              RHL_INPUTFIX_LEGACY=0 hides the normal mouse messages from the
 *              game so it only sees the pointer messages (for testing),
 *              RHL_INPUTFIX_CURSOR=0 turns off the cursor refresh,
 *              RHL_INPUTFIX_RAW=0 turns off the raw mouse corrections,
 *              RHL_INPUTFIX_TRACE=1 (with logging) records every mouse message.
 *
 * Part of the Runehaven Launcher (unofficial) – MIT License.
 */
#include <windows.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#include "inputfix.h"

HMODULE rhl_self;

static DWORD main_thread;
static BOOL logging;
static HANDLE log_file = INVALID_HANDLE_VALUE;
static LONG log_lines;
static CRITICAL_SECTION lock;

/* ------------------------------------------------------------------ log */

void rhl_log(const char *fmt, ...) {
    if (!logging || log_file == INVALID_HANDLE_VALUE) return;
    if (InterlockedIncrement(&log_lines) > 20000) return;
    char buf[512];
    int n = snprintf(buf, sizeof buf, "[%lu] ", GetTickCount());
    va_list ap;
    va_start(ap, fmt);
    n += vsnprintf(buf + n, sizeof buf - n - 2, fmt, ap);
    va_end(ap);
    if (n > (int)sizeof buf - 2) n = sizeof buf - 2;
    buf[n++] = '\n';
    DWORD written;
    WriteFile(log_file, buf, n, &written, NULL);
}

/* RHL_INPUTFIX_LOG=1 logs to %TEMP%\rhl-inputfix.log, any other value is
 * taken as the log file's path. Returns FALSE when logging is off. */
static BOOL open_log(void) {
    char path[MAX_PATH];
    DWORD n = GetEnvironmentVariableA("RHL_INPUTFIX_LOG", path, sizeof path);
    if (n == 0 || n >= sizeof path || strcmp(path, "0") == 0) return FALSE;
    if (strcmp(path, "1") == 0) {
        DWORD len = GetTempPathA(MAX_PATH, path);
        if (len == 0 || len > MAX_PATH - 20) return FALSE;
        strcat(path, "rhl-inputfix.log");
    }
    log_file = CreateFileA(path, GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_ALWAYS,
                           FILE_ATTRIBUTE_NORMAL, NULL);
    return log_file != INVALID_HANDLE_VALUE;
}

static BOOL env_is(const char *name, const char *value) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA(name, buf, sizeof buf);
    return n > 0 && n < sizeof buf && strcmp(buf, value) == 0;
}

/* ------------------------------------------------------- pointer state */

#define MOUSE_POINTER_ID 1
#define QUEUE_LEN 256

static BOOL mouse_in_pointer;
static POINTER_INFO current;            /* state of the message being handled */
static POINTER_INFO queue[QUEUE_LEN];   /* posted but not yet retrieved */
static unsigned q_head, q_tail;
static UINT32 frame_id;
static BOOL inside;                     /* pointer is over a game window */
static POINT last_pt;                   /* screen position of the last mouse message */
static BOOL have_last_pt;
static HHOOK msg_hook;
static BOOL trace_mouse;  /* RHL_INPUTFIX_TRACE=1: log every mouse message */
static BOOL fix_raw = TRUE; /* RHL_INPUTFIX_RAW=0 turns off the raw mouse corrections */

static void queue_push(const POINTER_INFO *info) {
    queue[q_tail % QUEUE_LEN] = *info;
    q_tail++;
    if (q_tail - q_head > QUEUE_LEN) q_head = q_tail - QUEUE_LEN; /* drop oldest */
}

static BOOL queue_pop(POINTER_INFO *out) {
    if (q_head == q_tail) return FALSE;
    *out = queue[q_head % QUEUE_LEN];
    q_head++;
    return TRUE;
}

/* MK_* button bits (from a mouse message) → POINTER_FLAG_*BUTTON bits. */
static UINT32 button_flags(WORD keys) {
    UINT32 f = 0;
    if (keys & MK_LBUTTON) f |= POINTER_FLAG_FIRSTBUTTON;
    if (keys & MK_RBUTTON) f |= POINTER_FLAG_SECONDBUTTON;
    if (keys & MK_MBUTTON) f |= POINTER_FLAG_THIRDBUTTON;
    if (keys & MK_XBUTTON1) f |= POINTER_FLAG_FOURTHBUTTON;
    if (keys & MK_XBUTTON2) f |= POINTER_FLAG_FIFTHBUTTON;
    return f;
}

static BOOL is_pointer_message(UINT m) {
    return m == WM_POINTERUPDATE || m == WM_POINTERDOWN || m == WM_POINTERUP ||
           m == WM_POINTERENTER || m == WM_POINTERLEAVE || m == WM_POINTERWHEEL ||
           m == WM_POINTERHWHEEL;
}

/* Builds and posts one pointer message; the info is queued so that
 * GetPointerInfo returns it while the game handles that very message. */
static void post_pointer(HWND hwnd, UINT msg, UINT32 flags, POINTER_BUTTON_CHANGE_TYPE change,
                         POINT screen, DWORD time, WORD keys, INT32 wheel) {
    POINTER_INFO info;
    ZeroMemory(&info, sizeof info);
    info.pointerType = PT_MOUSE;
    info.pointerId = MOUSE_POINTER_ID;
    info.frameId = ++frame_id;
    info.pointerFlags = flags;
    info.hwndTarget = hwnd;
    info.ptPixelLocation = screen;
    info.ptPixelLocationRaw = screen;
    info.ptHimetricLocation = screen;
    info.ptHimetricLocationRaw = screen;
    info.dwTime = time;
    info.historyCount = 1;
    info.InputData = wheel;
    info.dwKeyStates = keys & (MK_SHIFT | MK_CONTROL);
    LARGE_INTEGER pc;
    QueryPerformanceCounter(&pc);
    info.PerformanceCount = (UINT64)pc.QuadPart;
    info.ButtonChangeType = change;

    WORD high = (msg == WM_POINTERWHEEL || msg == WM_POINTERHWHEEL) ? (WORD)(SHORT)wheel
                                                                     : (WORD)(flags & 0xFFFF);
    WPARAM wp = MAKEWPARAM(MOUSE_POINTER_ID, high);
    LPARAM lp = MAKELPARAM((SHORT)screen.x, (SHORT)screen.y);

    EnterCriticalSection(&lock);
    queue_push(&info);
    if (!PostMessageW(hwnd, msg, wp, lp)) q_tail--; /* undo */
    LeaveCriticalSection(&lock);
}

/* ------------------------------------------------------ cursor refresh
 *
 * When macOS activates the game because of a click, it shows its own arrow,
 * but Wine still believes the game's cursor is on screen and skips setting
 * it again until the mouse leaves the window. Switching the cursor to a
 * different one and straight back makes Wine apply the game's cursor anew.
 * Wine combines cursor changes that are still pending, so each change is
 * delivered right away by letting Wine process its pending cursor
 * notification (a peek restricted to a private window, so no game message is
 * touched).
 */

#define WINE_SETCURSOR_MSG 0x80001FF1u /* last value of the filter range only */

static BOOL fix_cursor = TRUE;          /* RHL_INPUTFIX_CURSOR=0 turns this off */
static BOOL was_foreground;
static DWORD activated_at;
static int refreshes_done = 3;          /* refreshes after the last activation */
static const DWORD refresh_delays[3] = {0, 150, 500};
static HWND flush_window;

static void flush_cursor_change(void) {
    if (!flush_window)
        flush_window = CreateWindowExW(0, L"Message", NULL, 0, 0, 0, 0, 0, HWND_MESSAGE, NULL,
                                       NULL, NULL);
    if (!flush_window) return;
    MSG tmp;
    PeekMessageW(&tmp, flush_window, WM_MOUSELAST, WINE_SETCURSOR_MSG, PM_NOREMOVE);
}

static void refresh_cursor(void) {
    HCURSOR game = GetCursor();
    if (!game) return;
    HCURSOR other = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    if (other == game) other = LoadCursorW(NULL, (LPCWSTR)IDC_IBEAM);
    SetCursor(other);
    flush_cursor_change();
    SetCursor(game);
    flush_cursor_change();
    rhl_log("cursor refreshed (%p)", (void *)game);
}

/* After clicks macOS shows its arrow as well; that is handled on the Mac side
 * by the launcher's helper library (rml-macfix), which can re-apply the
 * cursor without the detour through Windows. */
static void track_activation(HWND hwnd) {
    BOOL fg = GetForegroundWindow() == GetAncestor(hwnd, GA_ROOT);
    DWORD now = GetTickCount();
    if (fg && !was_foreground) {
        activated_at = now;
        refreshes_done = 0;
        rhl_log("game window activated");
    }
    was_foreground = fg;
    if (fg && refreshes_done < 3 && now - activated_at >= refresh_delays[refreshes_done]) {
        refreshes_done++;
        refresh_cursor();
    }
}

static void handle_message(const MSG *m) {
    UINT msg = m->message;

    if (is_pointer_message(msg)) {
        POINTER_INFO info;
        EnterCriticalSection(&lock);
        if (queue_pop(&info)) current = info;
        LeaveCriticalSection(&lock);
        return;
    }
    if (msg < WM_MOUSEFIRST || msg > WM_MOUSELAST) {
        if (msg == WM_MOUSELEAVE && inside) {
            inside = FALSE;
            POINT pt;
            if (have_last_pt) pt = last_pt;
            else GetCursorPos(&pt);
            post_pointer(m->hwnd, WM_POINTERLEAVE, POINTER_FLAG_PRIMARY, POINTER_CHANGE_NONE, pt,
                         m->time, 0, 0);
        }
        return;
    }

    BOOL wheel = msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL;
    BOOL xbtn = msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP || msg == WM_XBUTTONDBLCLK;
    WORD keys = (wheel || xbtn) ? GET_KEYSTATE_WPARAM(m->wParam) : (WORD)m->wParam;

    POINT pt = {(SHORT)LOWORD(m->lParam), (SHORT)HIWORD(m->lParam)};
    if (!wheel) {
        ClientToScreen(m->hwnd, &pt);
        last_pt = pt;
        have_last_pt = TRUE;
    } else if (have_last_pt) {
        /* Wheel messages carry a screen position that under Wine's Retina
         * mode does not always match the other mouse messages; the game
         * would see the pointer jump with every scroll step. Scrolling does
         * not move the pointer, so the last known position is used. */
        pt = last_pt;
    }

    UINT32 buttons = button_flags(keys);
    UINT32 base = POINTER_FLAG_INRANGE | POINTER_FLAG_PRIMARY | buttons;
    if (buttons) base |= POINTER_FLAG_INCONTACT;

    if (!inside) {
        inside = TRUE;
        post_pointer(m->hwnd, WM_POINTERENTER, base | POINTER_FLAG_NEW, POINTER_CHANGE_NONE, pt,
                     m->time, keys, 0);
    }

    if (fix_cursor) track_activation(m->hwnd);

    switch (msg) {
    case WM_MOUSEMOVE:
        post_pointer(m->hwnd, WM_POINTERUPDATE, base | POINTER_FLAG_UPDATE, POINTER_CHANGE_NONE,
                     pt, m->time, keys, 0);
        break;
    case WM_LBUTTONDOWN: case WM_LBUTTONDBLCLK:
        post_pointer(m->hwnd, WM_POINTERDOWN, base | POINTER_FLAG_DOWN,
                     POINTER_CHANGE_FIRSTBUTTON_DOWN, pt, m->time, keys, 0);
        break;
    case WM_LBUTTONUP:
        post_pointer(m->hwnd, WM_POINTERUP, base | POINTER_FLAG_UP, POINTER_CHANGE_FIRSTBUTTON_UP,
                     pt, m->time, keys, 0);
        break;
    case WM_RBUTTONDOWN: case WM_RBUTTONDBLCLK:
        post_pointer(m->hwnd, WM_POINTERDOWN, base | POINTER_FLAG_DOWN,
                     POINTER_CHANGE_SECONDBUTTON_DOWN, pt, m->time, keys, 0);
        break;
    case WM_RBUTTONUP:
        post_pointer(m->hwnd, WM_POINTERUP, base | POINTER_FLAG_UP, POINTER_CHANGE_SECONDBUTTON_UP,
                     pt, m->time, keys, 0);
        break;
    case WM_MBUTTONDOWN: case WM_MBUTTONDBLCLK:
        post_pointer(m->hwnd, WM_POINTERDOWN, base | POINTER_FLAG_DOWN,
                     POINTER_CHANGE_THIRDBUTTON_DOWN, pt, m->time, keys, 0);
        break;
    case WM_MBUTTONUP:
        post_pointer(m->hwnd, WM_POINTERUP, base | POINTER_FLAG_UP, POINTER_CHANGE_THIRDBUTTON_UP,
                     pt, m->time, keys, 0);
        break;
    case WM_XBUTTONDOWN: case WM_XBUTTONDBLCLK: {
        BOOL first = GET_XBUTTON_WPARAM(m->wParam) == XBUTTON1;
        post_pointer(m->hwnd, WM_POINTERDOWN, base | POINTER_FLAG_DOWN,
                     first ? POINTER_CHANGE_FOURTHBUTTON_DOWN : POINTER_CHANGE_FIFTHBUTTON_DOWN,
                     pt, m->time, keys, 0);
        break;
    }
    case WM_XBUTTONUP: {
        BOOL first = GET_XBUTTON_WPARAM(m->wParam) == XBUTTON1;
        post_pointer(m->hwnd, WM_POINTERUP, base | POINTER_FLAG_UP,
                     first ? POINTER_CHANGE_FOURTHBUTTON_UP : POINTER_CHANGE_FIFTHBUTTON_UP, pt,
                     m->time, keys, 0);
        break;
    }
    case WM_MOUSEWHEEL:
        post_pointer(m->hwnd, WM_POINTERWHEEL, base | POINTER_FLAG_WHEEL, POINTER_CHANGE_NONE, pt,
                     m->time, keys, GET_WHEEL_DELTA_WPARAM(m->wParam));
        break;
    case WM_MOUSEHWHEEL:
        post_pointer(m->hwnd, WM_POINTERHWHEEL, base | POINTER_FLAG_HWHEEL, POINTER_CHANGE_NONE, pt,
                     m->time, keys, GET_WHEEL_DELTA_WPARAM(m->wParam));
        break;
    default:
        return;
    }
    if (msg != WM_MOUSEMOVE) rhl_log("mouse 0x%04x keys 0x%02x -> flags 0x%08x", msg, keys, base);
}

static BOOL drop_legacy_mouse; /* RHL_INPUTFIX_LEGACY=0: game only sees pointer messages */

static void trace(const MSG *m) {
    UINT msg = m->message;
    if (!(msg >= WM_MOUSEFIRST && msg <= WM_MOUSELAST) && msg != WM_INPUT) return;
    POINT cur;
    GetCursorPos(&cur);
    if (msg == WM_INPUT) {
        RAWINPUT raw;
        UINT size = sizeof raw;
        if (GetRawInputData((HRAWINPUT)m->lParam, RID_INPUT, &raw, &size, sizeof(RAWINPUTHEADER)) !=
                (UINT)-1 && raw.header.dwType == RIM_TYPEMOUSE)
            rhl_log("t=%lu INPUT flags=0x%x d=(%ld,%ld) btn=0x%x wheel=%d cursor=(%ld,%ld)", m->time,
                    raw.data.mouse.usFlags, raw.data.mouse.lLastX, raw.data.mouse.lLastY,
                    raw.data.mouse.usButtonFlags, (SHORT)raw.data.mouse.usButtonData, cur.x, cur.y);
        return;
    }
    POINT pt = {(SHORT)LOWORD(m->lParam), (SHORT)HIWORD(m->lParam)};
    POINT scr = pt;
    if (msg != WM_MOUSEWHEEL && msg != WM_MOUSEHWHEEL) ClientToScreen(m->hwnd, &scr);
    rhl_log("t=%lu msg=0x%04x wp=0x%08lx l=(%ld,%ld) screen=(%ld,%ld) cursor=(%ld,%ld) msgpos=(%d,%d)",
            m->time, msg, (unsigned long)m->wParam, pt.x, pt.y, scr.x, scr.y, cur.x, cur.y,
            (SHORT)LOWORD(m->pt.x), (SHORT)LOWORD(m->pt.y));
}

static LRESULT CALLBACK getmsg_proc(int code, WPARAM remove, LPARAM lparam) {
    /* Only act when the message is really taken from the queue; peeks without
     * PM_REMOVE would otherwise create duplicates. */
    if (code == HC_ACTION && (remove & PM_REMOVE)) {
        if (trace_mouse) trace((const MSG *)lparam);
        MSG *m = (MSG *)lparam;
        handle_message(m);
        if (drop_legacy_mouse && m->message >= WM_MOUSEFIRST && m->message <= WM_MOUSELAST)
            m->message = WM_NULL;
    }
    return CallNextHookEx(NULL, code, remove, lparam);
}

/* One hook per thread that may own the game window: the thread that loaded
 * the DLL (Unity's main thread) and the thread that turned pointer mode on. */
static DWORD hooked_threads[2];

static void hook_thread(DWORD thread) {
    for (int i = 0; i < 2; i++) {
        if (hooked_threads[i] == thread) return;
        if (hooked_threads[i] == 0) {
            HHOOK h = SetWindowsHookExW(WH_GETMESSAGE, getmsg_proc, rhl_self, thread);
            rhl_log("message hook for thread %lu: %s (error %lu)", thread, h ? "ok" : "FAILED",
                    h ? 0 : GetLastError());
            if (h) {
                hooked_threads[i] = thread;
                msg_hook = h;
            }
            return;
        }
    }
}

static void ensure_hook(void) {
    EnterCriticalSection(&lock);
    if (main_thread) hook_thread(main_thread);
    hook_thread(GetCurrentThreadId());
    LeaveCriticalSection(&lock);
}

/* --------------------------------------------- replacement functions */

static BOOL WINAPI fix_EnableMouseInPointer(BOOL enable) {
    rhl_log("EnableMouseInPointer(%d) from thread %lu", enable, GetCurrentThreadId());
    mouse_in_pointer = enable;
    ensure_hook();
    return TRUE;
}

static BOOL WINAPI fix_IsMouseInPointerEnabled(void) {
    return mouse_in_pointer;
}

static BOOL WINAPI fix_GetPointerType(UINT32 id, POINTER_INPUT_TYPE *type) {
    if (!type) {
        SetLastError(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    *type = PT_MOUSE;
    (void)id;
    return TRUE;
}

static BOOL copy_current(UINT32 id, POINTER_INFO *out) {
    if (!out || id != MOUSE_POINTER_ID) {
        SetLastError(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    EnterCriticalSection(&lock);
    *out = current;
    LeaveCriticalSection(&lock);
    return TRUE;
}

static BOOL WINAPI fix_GetPointerInfo(UINT32 id, POINTER_INFO *info) {
    return copy_current(id, info);
}

static BOOL WINAPI fix_GetPointerFrameInfo(UINT32 id, UINT32 *count, POINTER_INFO *infos) {
    if (!count) {
        SetLastError(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    if (!infos || *count < 1) {
        *count = 1;
        return infos ? (SetLastError(ERROR_INSUFFICIENT_BUFFER), FALSE) : TRUE;
    }
    *count = 1;
    return copy_current(id, infos);
}

static BOOL WINAPI fix_GetPointerInfoHistory(UINT32 id, UINT32 *count, POINTER_INFO *infos) {
    return fix_GetPointerFrameInfo(id, count, infos);
}

/* ------------------------------------------------ raw mouse corrections
 *
 * Unity turns the camera with raw mouse input (WM_INPUT). Under Wine's
 * Retina mode two kinds of raw events carry a movement that never happened:
 *
 *  - every scroll step and every click: Wine sends it together with the
 *    pointer position, rounded down from half pixels, which shows up as a
 *    move of about (-1,-1). The camera drifts up and to the left.
 *  - the first move after the game puts the pointer back into the window
 *    centre (SetCursorPos, e.g. when leaving the pause menu): Wine reports the
 *    whole jump as a mouse movement, so the camera swings away.
 *
 * Wine never combines a real movement with a scroll step or a click, so
 * those events lose their (tiny) movement. A movement equal to the last pointer jump within a
 * second of it is dropped once.
 */

static LONG warp_dx, warp_dy;
static DWORD warp_tick;
static BOOL warp_pending;
static HRAWINPUT last_dropped;

static BOOL near_value(LONG a, LONG b) {
    LONG diff = a > b ? a - b : b - a;
    LONG mag = b < 0 ? -b : b;
    return diff <= 8 + mag / 10;
}

/* Returns TRUE if the event's movement was removed. */
static BOOL correct_raw(RAWINPUT *raw, HRAWINPUT handle) {
    if (raw->header.dwType != RIM_TYPEMOUSE) return FALSE;
    RAWMOUSE *m = &raw->data.mouse;
    if (m->usFlags & MOUSE_MOVE_ABSOLUTE) return FALSE;
    if (handle && handle == last_dropped) {
        m->lLastX = m->lLastY = 0; /* the game reads the same event again */
        return TRUE;
    }
    if (m->usButtonFlags & (RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL)) {
        if (m->lLastX || m->lLastY) {
            m->lLastX = m->lLastY = 0;
            return TRUE;
        }
        return FALSE;
    }
    /* Clicks carry the pointer position as well, rounded the same way. */
    if (m->usButtonFlags && m->lLastX >= -1 && m->lLastX <= 1 && m->lLastY >= -1 &&
        m->lLastY <= 1) {
        if (m->lLastX || m->lLastY) {
            m->lLastX = m->lLastY = 0;
            return TRUE;
        }
        return FALSE;
    }
    if (!warp_pending || (!m->lLastX && !m->lLastY)) return FALSE;
    if (GetTickCount() - warp_tick > 1000) {
        warp_pending = FALSE;
        return FALSE;
    }
    if (near_value(m->lLastX, warp_dx) && near_value(m->lLastY, warp_dy)) {
        rhl_log("dropped raw move (%ld,%ld) after pointer jump (%ld,%ld)", m->lLastX, m->lLastY,
                warp_dx, warp_dy);
        m->lLastX = m->lLastY = 0;
        warp_pending = FALSE;
        if (handle) last_dropped = handle;
        return TRUE;
    }
    return FALSE;
}

static BOOL WINAPI fix_SetCursorPos(int x, int y) {
    POINT old;
    BOOL have_old = GetCursorPos(&old);
    BOOL ok = SetCursorPos(x, y);
    if (ok && have_old) {
        LONG dx = x - old.x, dy = y - old.y;
        if ((dx < 0 ? -dx : dx) + (dy < 0 ? -dy : dy) > 8) {
            warp_dx = dx;
            warp_dy = dy;
            warp_tick = GetTickCount();
            warp_pending = TRUE;
            if (trace_mouse) rhl_log("SetCursorPos(%d,%d) jump (%ld,%ld)", x, y, dx, dy);
        }
    }
    return ok;
}

static UINT WINAPI fix_GetRawInputData(HRAWINPUT h, UINT cmd, LPVOID data, PUINT size,
                                       UINT header_size) {
    UINT r = GetRawInputData(h, cmd, data, size, header_size);
    if (fix_raw && cmd == RID_INPUT && data && r != (UINT)-1 &&
        r >= sizeof(RAWINPUTHEADER) + sizeof(RAWMOUSE))
        correct_raw((RAWINPUT *)data, h);
    return r;
}

static UINT WINAPI fix_GetRawInputBuffer(PRAWINPUT data, PUINT size, UINT header_size) {
    UINT r = GetRawInputBuffer(data, size, header_size);
    if (fix_raw && data && r != (UINT)-1) {
        RAWINPUT *p = data;
        for (UINT i = 0; i < r; i++) {
            correct_raw(p, NULL);
            /* NEXTRAWINPUTBLOCK: blocks are aligned to 8 bytes */
            p = (RAWINPUT *)(((ULONG_PTR)p + p->header.dwSize + 7) & ~(ULONG_PTR)7);
        }
    }
    return r;
}

/* Only for the trace: what the game does with the cursor. */
static HCURSOR last_traced_cursor = (HCURSOR)-1;
static HCURSOR WINAPI fix_SetCursor(HCURSOR cursor) {
    if (trace_mouse && cursor != last_traced_cursor) {
        rhl_log("SetCursor(%p)", (void *)cursor);
        last_traced_cursor = cursor;
    }
    return SetCursor(cursor);
}

static int WINAPI fix_ShowCursor(BOOL show) {
    int count = ShowCursor(show);
    if (trace_mouse) rhl_log("ShowCursor(%d) -> %d", show, count);
    return count;
}

typedef FARPROC(WINAPI *GetProcAddress_fn)(HMODULE, LPCSTR);
static GetProcAddress_fn real_GetProcAddress;

static const struct {
    const char *name;
    void *fn;
} replacements[] = {
    {"EnableMouseInPointer", (void *)fix_EnableMouseInPointer},
    {"IsMouseInPointerEnabled", (void *)fix_IsMouseInPointerEnabled},
    {"GetPointerType", (void *)fix_GetPointerType},
    {"GetPointerInfo", (void *)fix_GetPointerInfo},
    {"GetPointerFrameInfo", (void *)fix_GetPointerFrameInfo},
    {"GetPointerInfoHistory", (void *)fix_GetPointerInfoHistory},
    {"SetCursorPos", (void *)fix_SetCursorPos},
    {"GetRawInputData", (void *)fix_GetRawInputData},
    {"GetRawInputBuffer", (void *)fix_GetRawInputBuffer},
    {"SetCursor", (void *)fix_SetCursor},
    {"ShowCursor", (void *)fix_ShowCursor},
};

static void *replacement_for(const char *name) {
    for (size_t i = 0; i < sizeof replacements / sizeof replacements[0]; i++)
        if (strcmp(replacements[i].name, name) == 0) return replacements[i].fn;
    return NULL;
}

/* Unity looks up some pointer functions at run time. */
static FARPROC WINAPI fix_GetProcAddress(HMODULE module, LPCSTR name) {
    if (HIWORD((ULONG_PTR)name) != 0 && module == GetModuleHandleW(L"user32.dll")) {
        void *fn = replacement_for(name);
        if (fn) {
            rhl_log("GetProcAddress(user32, %s) -> input fix", name);
            ensure_hook();
            return (FARPROC)fn;
        }
    }
    return real_GetProcAddress(module, name);
}

/* --------------------------------------------- import table patching */

static BOOL write_pointer(void **slot, void *value) {
    DWORD old;
    if (!VirtualProtect(slot, sizeof *slot, PAGE_READWRITE, &old)) return FALSE;
    *slot = value;
    VirtualProtect(slot, sizeof *slot, old, &old);
    return TRUE;
}

/* Replaces imports of `module` taken from `dll` by our functions. */
static int patch_imports(HMODULE module, const char *dll) {
    BYTE *base = (BYTE *)module;
    IMAGE_DOS_HEADER *dos = (IMAGE_DOS_HEADER *)base;
    if (dos->e_magic != IMAGE_DOS_SIGNATURE) return 0;
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + dos->e_lfanew);
    IMAGE_DATA_DIRECTORY dir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!dir.VirtualAddress) return 0;
    int patched = 0;
    for (IMAGE_IMPORT_DESCRIPTOR *d = (IMAGE_IMPORT_DESCRIPTOR *)(base + dir.VirtualAddress);
         d->Name; d++) {
        if (lstrcmpiA((const char *)(base + d->Name), dll) != 0) continue;
        IMAGE_THUNK_DATA *names =
            (IMAGE_THUNK_DATA *)(base + (d->OriginalFirstThunk ? d->OriginalFirstThunk : d->FirstThunk));
        IMAGE_THUNK_DATA *iat = (IMAGE_THUNK_DATA *)(base + d->FirstThunk);
        for (; names->u1.AddressOfData; names++, iat++) {
            if (IMAGE_SNAP_BY_ORDINAL(names->u1.Ordinal)) continue;
            const char *name = (const char *)((IMAGE_IMPORT_BY_NAME *)(base + names->u1.AddressOfData))->Name;
            void *fn = NULL;
            if (lstrcmpiA(dll, "kernel32.dll") == 0 && strcmp(name, "GetProcAddress") == 0)
                fn = (void *)fix_GetProcAddress;
            else if (lstrcmpiA(dll, "user32.dll") == 0)
                fn = replacement_for(name);
            if (fn && write_pointer((void **)&iat->u1.Function, fn)) {
                rhl_log("patched %s!%s", dll, name);
                patched++;
            }
        }
    }
    return patched;
}

static void install(void) {
    real_GetProcAddress = (GetProcAddress_fn)(void *)GetProcAddress(
        GetModuleHandleW(L"kernel32.dll"), "GetProcAddress");
    HMODULE unity = GetModuleHandleW(L"UnityPlayer.dll");
    if (!unity) {
        rhl_log("UnityPlayer.dll not loaded yet; nothing patched");
        return;
    }
    int n = patch_imports(unity, "user32.dll") + patch_imports(unity, "kernel32.dll");
    rhl_log("UnityPlayer.dll: %d imports redirected", n);
}

/* ------------------------------------------------------------- entry */

BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, LPVOID reserved) {
    (void)reserved;
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    rhl_self = instance;
    DisableThreadLibraryCalls(instance);
    InitializeCriticalSection(&lock);
    main_thread = GetCurrentThreadId();
    rhl_proxy_init();
    /* Other programs in the game folder (Unity's crash handler) load this DLL
     * too; they only get the forwarding, and they must not overwrite the log. */
    if (!GetModuleHandleW(L"UnityPlayer.dll")) return TRUE;
    logging = open_log();
    rhl_log("Runehaven Launcher input fix loaded (thread %lu)", main_thread);

    if (env_is("RHL_INPUTFIX", "0")) {
        rhl_log("disabled by RHL_INPUTFIX=0");
        return TRUE;
    }
    drop_legacy_mouse = env_is("RHL_INPUTFIX_LEGACY", "0");
    trace_mouse = logging && env_is("RHL_INPUTFIX_TRACE", "1");
    fix_raw = !env_is("RHL_INPUTFIX_RAW", "0");
    fix_cursor = !env_is("RHL_INPUTFIX_CURSOR", "0");
    current.pointerType = PT_MOUSE;
    current.pointerId = MOUSE_POINTER_ID;
    install();
    return TRUE;
}
