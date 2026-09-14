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

/* Keep runtime memory and serial output bounded even for large services. */
#define THESEUS_COVERAGE_GUARDS 65536U

static _Atomic uint32_t next_guard = 1;
static _Atomic unsigned char seen[THESEUS_COVERAGE_GUARDS];

/*
 * Clang and rustc call this once for every linked sanitizer-coverage guard
 * array. Guard numbers are deterministic within the build digest that scopes
 * every record. A DSO links its own hidden copy of this runtime, so a module
 * loaded with dlopen keeps an independent identity and hit set.
 */
__attribute__((visibility("hidden")))
void __sanitizer_cov_trace_pc_guard_init(uint32_t *start, uint32_t *stop) {
    if (start == stop || *start != 0) {
        return;
    }
    for (uint32_t *guard = start; guard < stop; guard++) {
        uint32_t id = atomic_fetch_add_explicit(&next_guard, 1, memory_order_relaxed);
        *guard = id < THESEUS_COVERAGE_GUARDS ? id : 0;
    }
}

/*
 * LLVM places trace-pc-guard callbacks at control-flow edges. The guard is the
 * stable edge identity for this exact build; the module-relative return PC is
 * retained so symbol files can explain the reached source location without
 * making ASLR part of replay identity.
 */
__attribute__((visibility("hidden")))
void __sanitizer_cov_trace_pc_guard(uint32_t *guard) {
    uint32_t id = *guard;
    if (id == 0 || id >= THESEUS_COVERAGE_GUARDS ||
        atomic_exchange_explicit(&seen[id], 1, memory_order_relaxed) != 0) {
        return;
    }

    uintptr_t pc = (uintptr_t)__builtin_return_address(0);
    Dl_info module;
    if (pc == 0 || dladdr((void *)pc, &module) == 0 || module.dli_fbase == NULL) {
        return;
    }

    uintptr_t offset = pc - (uintptr_t)module.dli_fbase;
    char record[288];
    int length = snprintf(
        record, sizeof(record),
        "THES:COV:v2:%s:%s:%s:%u:0x%lx\n",
        THESEUS_COVERAGE_PROCESS,
        THESEUS_COVERAGE_MODULE,
        THESEUS_COVERAGE_BUILD_SHA256,
        id,
        (unsigned long)offset);
    if (length > 0 && (size_t)length < sizeof(record)) {
        (void)write(STDERR_FILENO, record, (size_t)length);
    }
}
