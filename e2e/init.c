/* Theseus e2e proof: read guest-visible entropy, print it, power off.
 *
 * Boots as /init in a minimal initramfs. Deterministic runs (same seed) must
 * print identical hex; different seeds must differ.
 */
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
    /* virtio-rng (our seeded ChaCha device) — hwrng core, misc 10:183 */
    dump("hwrng", "/dev/hwrng", 10, 183);
    /* kernel CSPRNG — seeded from hwrng, so also deterministic under Theseus */
    dump("urandom", "/dev/urandom", 1, 9);
    printf("done\n");
    reboot(LINUX_REBOOT_CMD_POWER_OFF);
    return 0;
}
