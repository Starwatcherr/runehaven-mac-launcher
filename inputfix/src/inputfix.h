/* Runehaven Launcher input fix – shared declarations. MIT License. */
#pragma once
#include <windows.h>

extern HMODULE rhl_self;

BOOL rhl_proxy_init(void);
void rhl_log(const char *fmt, ...);
