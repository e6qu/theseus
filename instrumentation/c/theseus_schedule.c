/*
 * Copyright 2026 Adrian Mârza and contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

#define _GNU_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifndef THESEUS_SCHEDULE_PROCESS
#error "THESEUS_SCHEDULE_PROCESS is required"
#endif

#ifndef THESEUS_SCHEDULE_MODULE
#error "THESEUS_SCHEDULE_MODULE is required"
#endif

#ifndef THESEUS_SCHEDULE_BUILD_SHA256
#error "THESEUS_SCHEDULE_BUILD_SHA256 is required"
#endif

#define THESEUS_SCHEDULE_THREADS 32U
#define THESEUS_SCHEDULE_CHOICES 128U
#define THESEUS_SCHEDULE_DECISIONS 8192U
#define THESEUS_NO_THREAD UINT32_MAX

static pthread_mutex_t scheduler_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t scheduler_changed = PTHREAD_COND_INITIALIZER;
static pthread_once_t scheduler_once = PTHREAD_ONCE_INIT;
static uint32_t choices[THESEUS_SCHEDULE_CHOICES];
static uint32_t choice_count;
static uint32_t choice_cursor;
static unsigned char prefix_mode;
static uint32_t active_mask;
static uint32_t ended_mask;
static uint32_t current_thread = THESEUS_NO_THREAD;
static uint32_t next_thread_id = 1;
static uint64_t decision_count;
static pthread_t thread_handles[THESEUS_SCHEDULE_THREADS];
static uint32_t join_waiters[THESEUS_SCHEDULE_THREADS];
static unsigned char handle_known[THESEUS_SCHEDULE_THREADS];
static __thread uint32_t thread_id = THESEUS_NO_THREAD;

struct theseus_start {
    void *(*function)(void *);
    void *argument;
    uint32_t id;
};

extern int __real_pthread_create(
    pthread_t *, const pthread_attr_t *, void *(*)(void *), void *);
extern int __real_pthread_join(pthread_t, void **);

__attribute__((noreturn)) static void fatal(const char *message) {
    char output[256];
    int length = snprintf(output, sizeof(output), "THES:SCHED:ERROR:%s\n", message);
    if (length > 0 && (size_t)length < sizeof(output)) {
        (void)write(STDERR_FILENO, output, (size_t)length);
    }
    _exit(125);
}

static void initialize_scheduler(void) {
    const char *schedule = getenv("THESEUS_THREAD_SCHEDULE");
    const char *mode = getenv("THESEUS_THREAD_SCHEDULE_MODE");
    prefix_mode = mode != NULL && strcmp(mode, "runnable_prefix") == 0;
    if (mode != NULL && !prefix_mode) {
        fatal("THESEUS_THREAD_SCHEDULE_MODE must be runnable_prefix");
    }
    if (schedule == NULL || (!prefix_mode && *schedule == '\0')) {
        fatal("THESEUS_THREAD_SCHEDULE is required");
    }
    while (*schedule != '\0') {
        if (choice_count == THESEUS_SCHEDULE_CHOICES) {
            fatal("schedule has more than 128 choices");
        }
        errno = 0;
        char *end = NULL;
        unsigned long id = strtoul(schedule, &end, 10);
        if (errno != 0 || end == schedule || id >= THESEUS_SCHEDULE_THREADS) {
            fatal("schedule choices must be thread ids from 0 through 31");
        }
        choices[choice_count++] = (uint32_t)id;
        if (*end == '\0') {
            break;
        }
        if (*end != ',') {
            fatal("schedule choices must be comma-separated");
        }
        schedule = end + 1;
        if (*schedule == '\0') {
            fatal("schedule must not end with a comma");
        }
    }
    for (uint32_t id = 0; id < THESEUS_SCHEDULE_THREADS; id++) {
        join_waiters[id] = THESEUS_NO_THREAD;
    }
}

static uint32_t first_active(void) {
    for (uint32_t id = 0; id < THESEUS_SCHEDULE_THREADS; id++) {
        if ((active_mask & (1U << id)) != 0) {
            return id;
        }
    }
    return THESEUS_NO_THREAD;
}

static uint32_t choose_active(void) {
    if (prefix_mode) {
        /* Prefixes advance only at real choices, so their positions remain
         * meaningful when a thread blocks or exits between scheduling points. */
        if ((active_mask & (active_mask - 1U)) == 0) {
            return first_active();
        }
        if (choice_cursor == choice_count) {
            return first_active();
        }
        uint32_t selected = choices[choice_cursor++];
        if ((active_mask & (1U << selected)) == 0) {
            fatal("runnable prefix selected a thread outside the runnable set");
        }
        return selected;
    }
    for (uint32_t scanned = 0; scanned < choice_count; scanned++) {
        uint32_t selected = choices[choice_cursor];
        choice_cursor = (choice_cursor + 1) % choice_count;
        if ((active_mask & (1U << selected)) != 0) {
            return selected;
        }
    }
    return first_active();
}

static uintptr_t module_offset(void *pc) {
    Dl_info module;
    if (pc == NULL || dladdr(pc, &module) == 0 || module.dli_fbase == NULL) {
        return 0;
    }
    return (uintptr_t)pc - (uintptr_t)module.dli_fbase;
}

static void select_next(uint32_t from, uintptr_t offset) {
    uint32_t selected = choose_active();
    current_thread = selected;
    if (selected == THESEUS_NO_THREAD) {
        pthread_cond_broadcast(&scheduler_changed);
        return;
    }
    if (decision_count == THESEUS_SCHEDULE_DECISIONS) {
        fatal("execution exceeded 8192 scheduling decisions");
    }
    char record[320];
    int length = snprintf(
        record, sizeof(record),
        "THES:SCHED:v1:%s:%s:%s:%lu:%u:0x%08x:%u:0x%lx\n",
        THESEUS_SCHEDULE_PROCESS,
        THESEUS_SCHEDULE_MODULE,
        THESEUS_SCHEDULE_BUILD_SHA256,
        (unsigned long)decision_count,
        from,
        active_mask,
        selected,
        (unsigned long)offset);
    decision_count++;
    if (length <= 0 || (size_t)length >= sizeof(record)) {
        fatal("scheduling record exceeded its fixed buffer");
    }
    (void)write(STDERR_FILENO, record, (size_t)length);
    pthread_cond_broadcast(&scheduler_changed);
}

/*
 * GCC inserts this callback at application basic-block entries. The callback
 * serializes one block at a time according to the explicit repeating thread
 * schedule. The runtime is compiled separately, so its own bookkeeping is not
 * instrumented. Programs that block outside pthread_join are out of scope.
 */
void __sanitizer_cov_trace_pc(void) {
    pthread_once(&scheduler_once, initialize_scheduler);
    pthread_mutex_lock(&scheduler_lock);
    if (thread_id == THESEUS_NO_THREAD) {
        thread_id = 0;
        active_mask |= 1U;
        if (current_thread == THESEUS_NO_THREAD) {
            current_thread = 0;
        }
    }
    while (current_thread != thread_id) {
        pthread_cond_wait(&scheduler_changed, &scheduler_lock);
    }
    select_next(thread_id, module_offset(__builtin_return_address(0)));
    while (current_thread != thread_id) {
        pthread_cond_wait(&scheduler_changed, &scheduler_lock);
    }
    pthread_mutex_unlock(&scheduler_lock);
}

static void *run_thread(void *opaque) {
    struct theseus_start start = *(struct theseus_start *)opaque;
    free(opaque);
    thread_id = start.id;
    void *result = start.function(start.argument);

    pthread_mutex_lock(&scheduler_lock);
    ended_mask |= 1U << thread_id;
    active_mask &= ~(1U << thread_id);
    uint32_t waiter = join_waiters[thread_id];
    if (waiter != THESEUS_NO_THREAD) {
        active_mask |= 1U << waiter;
        join_waiters[thread_id] = THESEUS_NO_THREAD;
    }
    if (current_thread == thread_id) {
        select_next(thread_id, 0);
    }
    pthread_mutex_unlock(&scheduler_lock);
    return result;
}

int __wrap_pthread_create(
    pthread_t *thread,
    const pthread_attr_t *attributes,
    void *(*function)(void *),
    void *argument) {
    pthread_once(&scheduler_once, initialize_scheduler);
    struct theseus_start *start = malloc(sizeof(*start));
    if (start == NULL) {
        return ENOMEM;
    }

    pthread_mutex_lock(&scheduler_lock);
    if (next_thread_id == THESEUS_SCHEDULE_THREADS) {
        pthread_mutex_unlock(&scheduler_lock);
        free(start);
        return EAGAIN;
    }
    start->function = function;
    start->argument = argument;
    start->id = next_thread_id++;
    uint32_t id = start->id;
    active_mask |= 1U << start->id;
    pthread_mutex_unlock(&scheduler_lock);

    int result = __real_pthread_create(thread, attributes, run_thread, start);
    if (result != 0) {
        pthread_mutex_lock(&scheduler_lock);
        active_mask &= ~(1U << id);
        pthread_mutex_unlock(&scheduler_lock);
        free(start);
    } else {
        pthread_mutex_lock(&scheduler_lock);
        thread_handles[id] = *thread;
        handle_known[id] = 1;
        pthread_mutex_unlock(&scheduler_lock);
    }
    return result;
}

int __wrap_pthread_join(pthread_t target, void **result) {
    pthread_once(&scheduler_once, initialize_scheduler);
    pthread_mutex_lock(&scheduler_lock);
    uint32_t joining = thread_id;
    uint32_t target_id = THESEUS_NO_THREAD;
    for (uint32_t next = next_thread_id; next > 1; next--) {
        uint32_t candidate = next - 1;
        if (handle_known[candidate] && pthread_equal(thread_handles[candidate], target)) {
            target_id = candidate;
            break;
        }
    }
    if (joining != THESEUS_NO_THREAD && target_id == THESEUS_NO_THREAD) {
        pthread_mutex_unlock(&scheduler_lock);
        fatal("pthread_join target was not created by this scheduler");
    }
    if (joining == target_id) {
        pthread_mutex_unlock(&scheduler_lock);
        fatal("a scheduled thread cannot join itself");
    }
    int waiting = joining != THESEUS_NO_THREAD
        && (ended_mask & (1U << target_id)) == 0;
    if (waiting) {
        if (join_waiters[target_id] != THESEUS_NO_THREAD) {
            pthread_mutex_unlock(&scheduler_lock);
            fatal("more than one thread joined the same target");
        }
        join_waiters[target_id] = joining;
        active_mask &= ~(1U << joining);
        if (current_thread == joining) {
            select_next(joining, 0);
        }
    }
    pthread_mutex_unlock(&scheduler_lock);

    int status = __real_pthread_join(target, result);

    pthread_mutex_lock(&scheduler_lock);
    if (target_id != THESEUS_NO_THREAD && status == 0) {
        handle_known[target_id] = 0;
    }
    if (waiting) {
        while (current_thread != joining) {
            pthread_cond_wait(&scheduler_changed, &scheduler_lock);
        }
    }
    pthread_mutex_unlock(&scheduler_lock);
    return status;
}
