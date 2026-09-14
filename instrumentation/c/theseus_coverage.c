/*
 * Copyright 2026 Adrian Mârza and contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

#define _GNU_SOURCE

#include <dlfcn.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

#ifndef THESEUS_COVERAGE_PROCESS
#error "THESEUS_COVERAGE_PROCESS is required"
#endif

#ifndef THESEUS_COVERAGE_MODULE
#error "THESEUS_COVERAGE_MODULE is required"
#endif

#ifndef THESEUS_COVERAGE_BUILD_SHA256
#error "THESEUS_COVERAGE_BUILD_SHA256 is required"
#endif

#define THESEUS_COVERAGE_SLOTS 8192

static _Atomic uintptr_t seen[THESEUS_COVERAGE_SLOTS];

/*
 * GCC inserts this callback at basic-block entries. The runtime itself is
 * compiled separately without coverage instrumentation, preventing recursive
 * callbacks. A fixed lock-free set bounds memory and emits each address once.
 */
void __sanitizer_cov_trace_pc(void) {
    uintptr_t pc = (uintptr_t)__builtin_return_address(0);
    Dl_info module;
    if (pc == 0 || dladdr((void *)pc, &module) == 0 || module.dli_fbase == NULL) {
        return;
    }

    uintptr_t offset = pc - (uintptr_t)module.dli_fbase;
    uintptr_t value = offset + 1;
    size_t slot = (size_t)((offset ^ (offset >> 17)) % THESEUS_COVERAGE_SLOTS);
    for (size_t probe = 0; probe < THESEUS_COVERAGE_SLOTS; probe++) {
        uintptr_t present = atomic_load_explicit(&seen[slot], memory_order_relaxed);
        if (present == value) {
            return;
        }
        if (present == 0 && atomic_compare_exchange_strong_explicit(
                                &seen[slot], &present, value,
                                memory_order_relaxed, memory_order_relaxed)) {
            char record[256];
            int length = snprintf(
                record, sizeof(record),
                "THES:COV:v1:%s:%s:%s:0x%lx\n",
                THESEUS_COVERAGE_PROCESS,
                THESEUS_COVERAGE_MODULE,
                THESEUS_COVERAGE_BUILD_SHA256,
                (unsigned long)offset);
            if (length > 0 && (size_t)length < sizeof(record)) {
                (void)write(STDERR_FILENO, record, (size_t)length);
            }
            return;
        }
        slot = (slot + 1) % THESEUS_COVERAGE_SLOTS;
    }
}
