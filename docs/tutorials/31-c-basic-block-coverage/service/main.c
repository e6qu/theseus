#include <stdio.h>
#include <stdlib.h>

static const char *classify(int value) {
    if (value == 7) {
        return "seven";
    }
    if ((value & 1) == 0) {
        return "even";
    }
    return "odd";
}

int main(int argc, char **argv) {
    if (argc != 2) {
        return 2;
    }
    printf("class=%s\n", classify(atoi(argv[1])));
    return 0;
}
