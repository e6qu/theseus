/* Tutorial 02: control the random. Reads four u64 values from the guest's
 * entropy device and prints them as decimal numbers, then powers off.
 * With a script of [1, 2, 3, 4] (as little-endian u64s), the guest sees
 * those four values before the device resumes the seeded stream.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <linux/reboot.h>

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    mknod("/dev/hwrng", S_IFCHR | 0444, makedev(10, 183));

    int fd = open("/dev/hwrng", O_RDONLY);
    if (fd < 0) {
        printf("hwrng: OPEN FAILED\n");
        reboot(LINUX_REBOOT_CMD_POWER_OFF);
        return 1;
    }

    uint64_t values[4];
    ssize_t n = read(fd, values, sizeof values);
    close(fd);
    if (n != (ssize_t)sizeof values) {
        printf("hwrng: SHORT READ %zd\n", n);
        reboot(LINUX_REBOOT_CMD_POWER_OFF);
        return 1;
    }

    printf("random() =");
    for (int i = 0; i < 4; i++)
        printf(" %lu", (unsigned long)values[i]);
    printf("\n");
    reboot(LINUX_REBOOT_CMD_POWER_OFF);
    return 0;
}
