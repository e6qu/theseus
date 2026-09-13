/* Theseus e2e probe: read Linux's standard random devices and power off. */
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <linux/reboot.h>

static void dump(const char *label, const char *path, int major, int minor) {
    unsigned char buf[64];
    /* initramfs has no devtmpfs here; create the node ourselves. */
    mknod(path, S_IFCHR | 0444, makedev(major, minor));
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        printf("%s: OPEN FAILED\n", label);
        return;
    }
    ssize_t n = read(fd, buf, sizeof buf);
    close(fd);
    printf("%s (%zd bytes): ", label, n);
    for (ssize_t i = 0; i < n; i++)
        printf("%02x", buf[i]);
    printf("\n");
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("theseus-e2e init\n");
    dump("random", "/dev/random", 1, 8);
    dump("urandom", "/dev/urandom", 1, 9);
    printf("done\n");
    reboot(LINUX_REBOOT_CMD_POWER_OFF);
    return 0;
}
