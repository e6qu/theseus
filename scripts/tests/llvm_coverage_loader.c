#include <dlfcn.h>
#include <stdlib.h>

typedef int (*classify_fn)(int);

int main(int argc, char **argv) {
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_NODELETE);
    if (library == NULL) {
        return 2;
    }
    classify_fn classify = (classify_fn)dlsym(library, "classify");
    if (classify == NULL) {
        return 3;
    }
    return classify(argc == 3 ? atoi(argv[2]) : 0) == 70 ? 0 : 1;
}
