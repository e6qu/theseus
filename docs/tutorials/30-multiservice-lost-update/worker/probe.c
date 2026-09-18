#include <netdb.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    struct addrinfo hints = {
        .ai_family = AF_INET,
        .ai_socktype = SOCK_DGRAM,
    };
    struct addrinfo *addresses = NULL;
    int status = getaddrinfo("counter", "9", &hints, &addresses);
    if (status != 0) {
        fprintf(stderr, "probe: %s\n", gai_strerror(status));
        return 1;
    }

    int socket_fd = socket(addresses->ai_family, addresses->ai_socktype, 0);
    if (socket_fd == -1) {
        perror("probe: socket");
        freeaddrinfo(addresses);
        return 1;
    }

    static const char payload[] = "partition-probe";
    ssize_t sent = sendto(socket_fd, payload, sizeof(payload) - 1, 0,
                          addresses->ai_addr, addresses->ai_addrlen);
    if (sent != (ssize_t)(sizeof(payload) - 1)) {
        perror("probe: sendto");
        close(socket_fd);
        freeaddrinfo(addresses);
        return 1;
    }

    close(socket_fd);
    freeaddrinfo(addresses);
    return 0;
}
