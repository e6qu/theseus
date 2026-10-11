#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int choice(const char *name, int upper) {
    const char *cursor = getenv("THESEUS_CHOICES");
    size_t length = strlen(name);

    while (cursor != NULL && *cursor != '\0') {
        if (strncmp(cursor, name, length) == 0 && cursor[length] == '=') {
            char *end = NULL;
            long selected = strtol(cursor + length + 1, &end, 10);
            if (end != cursor + length + 1 && selected >= 0 && selected < upper &&
                (*end == ',' || *end == '\0')) {
                fprintf(stderr, "THES:CHOICE:%s:%d:%ld\n", name, upper, selected);
                return (int)selected;
            }
        }
        cursor = strchr(cursor, ',');
        if (cursor != NULL) {
            cursor++;
        }
    }

    fprintf(stderr, "missing choice %s\n", name);
    exit(2);
}

int main(void) {
    int lane = choice("lane", 2);
    int phase = choice("phase", 2);
    int level = choice("level", 2);
    const char *status =
        lane == 1 && phase == 1 && level == 0 ? "corrupt" : "ok";
    printf("{\"lane\":%d,\"phase\":%d,\"level\":%d,\"status\":\"%s\"}\n",
           lane, phase, level, status);
    return 0;
}
