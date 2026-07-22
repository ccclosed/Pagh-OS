#include <poll.h>
#include <stdint.h>
#include <unistd.h>

static int fail(int code) { return code; }

int main(void) {
    int fds[2];
    if (pipe(fds) != 0) return fail(10);

    const char message[] = "pagh-pipe-ok";
    if (write(fds[1], message, sizeof(message)) != (ssize_t)sizeof(message)) return fail(11);

    struct pollfd p = { .fd = fds[0], .events = POLLIN, .revents = 0 };
    if (poll(&p, 1, 0) != 1 || !(p.revents & POLLIN)) return fail(12);

    char out[sizeof(message)] = {0};
    if (read(fds[0], out, sizeof(out)) != (ssize_t)sizeof(out)) return fail(13);
    for (unsigned i = 0; i < sizeof(message); ++i)
        if (out[i] != message[i]) return fail(14);

    close(fds[1]);
    p.revents = 0;
    if (poll(&p, 1, 0) != 1 || !(p.revents & POLLHUP)) return fail(15);
    close(fds[0]);
    return 0;
}
