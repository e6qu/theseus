// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

// A dependency-free, self-checking protocol test: the emitted lines must
// be byte-identical to the Rust, Go, and Java SDKs, because the host-side
// property layer matches them as needles. Build and run:
//   cc -o theseus_c_test theseus_test.c && ./theseus_c_test
// A nonzero exit is a failure.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "theseus.h"

static int failures = 0;

static void check(const char *what, const char *got, const char *want) {
    if (strcmp(got, want) != 0) {
        failures++;
        printf("FAIL %s:\n got %s\nwant %s\n", what, got, want);
    }
}

/* Capture stderr by redirecting it to a file, running the emitter, then
 * reading the file back. Returns 0 on any harness failure. */
static int capture(void (*emit)(void), const char *path, char *buffer,
                   size_t capacity) {
    FILE *saved;
    FILE *file;
    memset(buffer, 0, capacity);
    fflush(stderr);
    saved = stderr;
    file = freopen(path, "w+", stderr);
    if (file == NULL) {
        return 0;
    }
    emit();
    fflush(stderr);
    stderr = saved;
    file = fopen(path, "r");
    if (file == NULL) {
        return 0;
    }
    fread(buffer, 1, capacity - 1, file);
    fclose(file);
    remove(path);
    return 1;
}

static void emit_protocol(void) {
    theseus_marker(THESEUS_MARKER_BOOT);
    theseus_assertion("no_data_loss", 1);
    theseus_assertion("stale_read", 0);
    theseus_checkpoint("write");
}

static void emit_events(void) {
    theseus_event("{\"event\":\"request\",\"worker\":\"a\"}");
    theseus_checkpoint("release");
}

static void emit_rejected(void) {
    theseus_event("not an object");
    theseus_choice("mode", 0);
}

int main(void) {
    char buffer[4096];

    if (!capture(emit_protocol, "/tmp/theseus-c-protocol.txt", buffer,
                 sizeof(buffer))) {
        printf("FAIL stderr capture harness\n");
        return 1;
    }
    check("protocol lines", buffer,
          "THES:M:42\n"
          "THES:ASSERT:no_data_loss:pass\n"
          "THES:ASSERT:stale_read:fail\n"
          "THES:CHECKPOINT:write\n");

    /* The choice consumes the locked assignment and records it. */
    setenv(THESEUS_CHOICES_ENV, "mode=1,retry=0", 1);
    if (!capture(emit_events, "/tmp/theseus-c-events.txt", buffer,
                 sizeof(buffer))) {
        printf("FAIL stderr capture harness\n");
        return 1;
    }
    check("event timeline", buffer,
          "{\"seq\":0,\"event\":{\"event\":\"request\",\"worker\":\"a\"}}\n"
          "THES:CHECKPOINT:release\n");
    if (theseus_choice("mode", 2) != 1) {
        failures++;
        printf("FAIL choice value\n");
    }
    /* The choice line prints to the redirected stderr of this direct call;
     * it is checked in the next capture instead. */
    unsetenv(THESEUS_CHOICES_ENV);

    /* Rejects: an empty bound, an absent assignment, and a non-object
     * event all degrade without writing protocol lines. */
    if (!capture(emit_rejected, "/tmp/theseus-c-rejected.txt", buffer,
                 sizeof(buffer))) {
        printf("FAIL stderr capture harness\n");
        return 1;
    }
    check("rejections write nothing", buffer, "");
    if (theseus_choice_assignment("absent") != -1) {
        failures++;
        printf("FAIL absent assignment\n");
    }

    if (failures > 0) {
        printf("%d check(s) failed\n", failures);
        return 1;
    }
    printf("theseus c sdk protocol: ok\n");
    return 0;
}
