#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/reboot.h>
#include <termios.h>
#include <unistd.h>

int main(void) {
    static const char message[] = "finished\n";
    int serial = open("/dev/ttyS0", O_WRONLY | O_NOCTTY);
    if (serial < 0) {
        perror("open /dev/ttyS0");
        return 1;
    }

    size_t offset = 0;
    while (offset < sizeof(message) - 1) {
        ssize_t written = write(serial, message + offset, sizeof(message) - 1 - offset);
        if (written < 0 && errno == EINTR) {
            continue;
        }
        if (written <= 0) {
            perror("write /dev/ttyS0");
            return 1;
        }
        offset += (size_t)written;
    }
    if (tcdrain(serial) != 0) {
        perror("tcdrain /dev/ttyS0");
        return 1;
    }
    close(serial);

    if (reboot(RB_AUTOBOOT) != 0) {
        perror("reboot");
    }
    return 1;
}
