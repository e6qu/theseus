const char *classify(int value) {
    if (value == 7) {
        return "seven";
    }
    if ((value & 1) == 0) {
        return "even";
    }
    return "odd";
}
