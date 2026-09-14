#include <stdio.h>
#include <stdlib.h>

static int classify(int value) {
    if (value == 7) {
        puts("seven");
        return 1;
    }
    puts("other");
    return 0;
}

int main(int argc, char **argv) {
    return classify(argc == 2 ? atoi(argv[1]) : 0);
}
