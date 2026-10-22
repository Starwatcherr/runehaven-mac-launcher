/*
 * version.dll proxy: every export jumps straight to the same function in the
 * system's version.dll, so the game and its libraries keep working normally.
 * Loading under this name is what gets the input fix into the game process
 * (UnityPlayer.dll imports VERSION.dll).
 *
 * Part of the Runehaven Launcher (unofficial) – MIT License.
 */
#include <windows.h>

#include "inputfix.h"

/* Fallback for every export until the real DLL is loaded (or if it cannot be). */
static DWORD WINAPI missing_export(void) {
    SetLastError(ERROR_PROC_NOT_FOUND);
    return 0;
}

/*
 * One pointer and one tiny jump stub per export. The stub is a tail jump, so
 * the call signature does not matter and arguments pass through untouched.
 */
#define FORWARD(name)                                                       \
    void *rhl_real_##name = (void *)missing_export;                         \
    __asm__(".globl " #name "\n" #name ":\n\tjmp *rhl_real_" #name "(%rip)\n");

FORWARD(GetFileVersionInfoA)
FORWARD(GetFileVersionInfoByHandle)
FORWARD(GetFileVersionInfoExA)
FORWARD(GetFileVersionInfoExW)
FORWARD(GetFileVersionInfoSizeA)
FORWARD(GetFileVersionInfoSizeExA)
FORWARD(GetFileVersionInfoSizeExW)
FORWARD(GetFileVersionInfoSizeW)
FORWARD(GetFileVersionInfoW)
FORWARD(VerFindFileA)
FORWARD(VerFindFileW)
FORWARD(VerInstallFileA)
FORWARD(VerInstallFileW)
FORWARD(VerLanguageNameA)
FORWARD(VerLanguageNameW)
FORWARD(VerQueryValueA)
FORWARD(VerQueryValueW)

#define RESOLVE(name)                                                       \
    do {                                                                    \
        FARPROC p = GetProcAddress(real, #name);                            \
        if (p) rhl_real_##name = (void *)p;                                 \
    } while (0)

/* Loads the system version.dll and points every export at it. */
BOOL rhl_proxy_init(void) {
    WCHAR path[MAX_PATH];
    UINT len = GetSystemDirectoryW(path, MAX_PATH);
    if (len == 0 || len > MAX_PATH - 13) return FALSE;
    lstrcatW(path, L"\\version.dll");
    HMODULE real = LoadLibraryW(path);
    if (!real || real == rhl_self) {
        rhl_log("proxy: could not load the system version.dll");
        return FALSE;
    }
    RESOLVE(GetFileVersionInfoA);
    RESOLVE(GetFileVersionInfoByHandle);
    RESOLVE(GetFileVersionInfoExA);
    RESOLVE(GetFileVersionInfoExW);
    RESOLVE(GetFileVersionInfoSizeA);
    RESOLVE(GetFileVersionInfoSizeExA);
    RESOLVE(GetFileVersionInfoSizeExW);
    RESOLVE(GetFileVersionInfoSizeW);
    RESOLVE(GetFileVersionInfoW);
    RESOLVE(VerFindFileA);
    RESOLVE(VerFindFileW);
    RESOLVE(VerInstallFileA);
    RESOLVE(VerInstallFileW);
    RESOLVE(VerLanguageNameA);
    RESOLVE(VerLanguageNameW);
    RESOLVE(VerQueryValueA);
    RESOLVE(VerQueryValueW);
    return TRUE;
}
