#include <cstdlib>
#include <string>

int main(int argc, char **argv) {
    const int value = argc == 2 ? std::atoi(argv[1]) : 0;
    const std::string result = value == 7 ? "seven" : "other";
    return result == "seven" ? 7 : 0;
}
