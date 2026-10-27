/* RML input fix – shared declarations. GPL-3.0-or-later, see LICENSE and NOTICE.md. */
#pragma once
#include <windows.h>

extern HMODULE rhl_self;

BOOL rhl_proxy_init(void);
void rhl_log(const char *fmt, ...);
