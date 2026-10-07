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
    int depth = choice("depth", 3);
    int strategy = choice("strategy", 2);
    const char *status =
        depth == 2 && strategy == 1 ? "corrupt" : "ok";
    printf("{\"depth\":%d,\"strategy\":%d,\"status\":\"%s\"}\n",
           depth, strategy, status);
    return 0;
}
