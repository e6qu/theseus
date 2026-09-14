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
    int mode = choice("mode", 2);
    int retry = choice("retry", 3);
    const char *status = mode == 1 && retry == 2 ? "corrupt" : "ok";
    printf("{\"mode\":%d,\"retry\":%d,\"status\":\"%s\"}\n",
           mode, retry, status);
    return 0;
}
