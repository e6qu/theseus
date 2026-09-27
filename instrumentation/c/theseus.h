// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

// The Theseus guest SDK for C and C++ services: the same serial-line
// vocabulary as the Rust, Go, and Java SDKs — markers, named runtime
// assertions, operation checkpoints, bounded structured choices consumed
// from THESEUS_CHOICES, and queued JSON event lines — written to stderr,
// where the host tails it as deterministic evidence.
//
// The header is dependency-free (stdio/stdlib/string only) so it links
// into any guest program without build system changes. Lines are
// byte-identical to the other SDKs because the host-side property layer
// matches them as needles.

#ifndef THESEUS_H
#define THESEUS_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Stable line prefixes and the structured-choice environment variable.
 * These match the Rust, Go, and Java SDKs exactly. */
#define THESEUS_MARKER_PREFIX "THES:M:"
#define THESEUS_ASSERTION_PREFIX "THES:ASSERT:"
#define THESEUS_CHECKPOINT_PREFIX "THES:CHECKPOINT:"
#define THESEUS_CHOICE_PREFIX "THES:CHOICE:"
#define THESEUS_CHOICES_ENV "THESEUS_CHOICES"

/* Boot and round markers, shared with the control-channel protocol. */
#define THESEUS_MARKER_BOOT 0x42
#define THESEUS_MARKER_DONE 0xFF

/* The queued JSON event batch: at most 8 events of 896 bytes each, well
 * inside a stack frame. Events are compact JSON object lines the property
 * layer's JSON predicates evaluate. */
#define THESEUS_EVENT_CAPACITY 8
#define THESEUS_EVENT_BYTES 896

static char theseus_event_objects[THESEUS_EVENT_CAPACITY][THESEUS_EVENT_BYTES];
static long theseus_event_count = 0;

/* Validate one identity field: 1-64 characters of [A-Za-z0-9._-], the
 * same contract the choice names and the coverage identities use. */
static int theseus_valid_identity(const char *value) {
    size_t length = 0;
    if (value == NULL) {
        return 0;
    }
    for (; value[length] != '\0'; length++) {
        int allowed = (value[length] >= 'a' && value[length] <= 'z') ||
                      (value[length] >= 'A' && value[length] <= 'Z') ||
                      (value[length] >= '0' && value[length] <= '9') ||
                      value[length] == '.' || value[length] == '_' ||
                      value[length] == '-';
        if (!allowed) {
            return 0;
        }
    }
    return length >= 1 && length <= 64;
}

/* Emit a marker byte; the host sees a "THES:M:xx" line. */
static void theseus_marker(unsigned char byte) {
    fprintf(stderr, THESEUS_MARKER_PREFIX "%02x\n", byte);
}

/* Report a named runtime assertion. The name must be stable across runs;
 * campaign properties match the line directly as an auditable property
 * witness rather than inferring state from host timing. */
static void theseus_assertion(const char *name, int passed) {
    fprintf(stderr, "%s%s:%s\n", THESEUS_ASSERTION_PREFIX, name,
            passed ? "pass" : "fail");
}

/* Look up one structured choice assignment in THESEUS_CHOICES. Returns
 * the selected value, or -1 when the assignment is absent or malformed —
 * replay fails early if the workload's decision contract changes. */
static long theseus_choice_assignment(const char *name) {
    const char *encoded = getenv(THESEUS_CHOICES_ENV);
    const char *cursor;
    size_t name_length;
    if (encoded == NULL) {
        return -1;
    }
    name_length = strlen(name);
    cursor = encoded;
    while (*cursor != '\0') {
        const char *equals = strchr(cursor, '=');
        const char *comma = strchr(cursor, ',');
        size_t segment = comma != NULL ? (size_t)(comma - cursor) : strlen(cursor);
        size_t key_length = equals != NULL ? (size_t)(equals - cursor) : 0;
        if (key_length == name_length && key_length <= segment &&
            strncmp(cursor, name, name_length) == 0) {
            return strtol(equals + 1, NULL, 10);
        }
        if (comma == NULL) {
            break;
        }
        cursor = comma + 1;
    }
    return -1;
}

/* Consume one named structured choice and record it immediately before
 * the workload uses the value. Theseus injects the exact assignment
 * through THESEUS_CHOICES; the name and bound are checked at the call
 * site so a replay fails early if the decision contract changes. Returns
 * the selected value, or -1 on an invalid name, bound, or assignment. */
static long theseus_choice(const char *name, long upper_exclusive) {
    long selected;
    if (upper_exclusive <= 0 || upper_exclusive > 256 ||
        !theseus_valid_identity(name)) {
        return -1;
    }
    selected = theseus_choice_assignment(name);
    if (selected < 0 || selected >= upper_exclusive) {
        return -1;
    }
    fprintf(stderr, "%s%s:%ld:%ld\n", THESEUS_CHOICE_PREFIX, name,
            upper_exclusive, selected);
    return selected;
}

/* Queue one application event: `json_object` is a complete JSON object
 * (it must start with '{' and end with '}') carrying the caller's fields.
 * The event is not written until theseus_event_flush or
 * theseus_checkpoint. Queuing beyond the capacity is a no-op: evidence
 * degrades instead of breaking the workload. */
static void theseus_event(const char *json_object) {
    size_t length;
    if (json_object == NULL || theseus_event_count >= THESEUS_EVENT_CAPACITY) {
        return;
    }
    length = strlen(json_object);
    if (length == 0 || length + 1 > THESEUS_EVENT_BYTES ||
        json_object[0] != '{' || json_object[length - 1] != '}') {
        return;
    }
    memcpy(theseus_event_objects[theseus_event_count], json_object,
           length + 1);
    theseus_event_count++;
}

/* Flush every queued application event as one compact JSON line per
 * event, in submission order under the deterministic "seq" number, and
 * clear the batch. The caller's object nests under "event" (C has no JSON
 * parser to merge keys), so property predicates address fields at
 * /event/... — the same evaluators, one level deeper. */
static void theseus_event_flush(void) {
    long index;
    for (index = 0; index < theseus_event_count; index++) {
        fprintf(stderr, "{\"seq\":%ld,\"event\":%s}\n", index,
                theseus_event_objects[index]);
        theseus_event_objects[index][0] = '\0';
    }
    theseus_event_count = 0;
}

/* Mark the end of one workload operation, giving applications a stable
 * serial checkpoint protocol without requiring the host to infer
 * progress. Queued application events flush first, so the ordered event
 * timeline stays inside the checkpoint sequence. */
static void theseus_checkpoint(const char *name) {
    theseus_event_flush();
    fprintf(stderr, "%s%s\n", THESEUS_CHECKPOINT_PREFIX, name);
}

#endif /* THESEUS_H */
