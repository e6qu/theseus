#include <dlfcn.h>
#include <cstdlib>
#include <iostream>

using classify_fn = const char *(*)(int);

int main(int argc, char **argv) {
    if (argc != 3) {
        return 2;
    }
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_NODELETE);
    if (library == nullptr) {
        return 3;
    }
    auto classify = reinterpret_cast<classify_fn>(dlsym(library, "classify"));
    if (classify == nullptr) {
        return 4;
    }
    std::cout << "class=" << classify(std::atoi(argv[2])) << '\n';
    return 0;
}
