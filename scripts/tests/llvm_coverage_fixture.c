#include <stdlib.h>

int main(int argc, char **argv) {
    int value = argc == 2 ? atoi(argv[1]) : 0;
    if (value == 7) {
        return 7;
    }
    return value & 1;
}
