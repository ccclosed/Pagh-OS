/*
 * net_probe.c — freestanding x86_64 Linux probe for the AF_INET bind/listen
 * port gate and the SOCK_RAW gate.
 *
 * WHY THIS EXISTS. The in-QEMU `selftest` routines run in ring 0 on kernel
 * pages, so they cannot pass a real user `struct sockaddr_in` through `bind(2)`
 * and cannot observe the errno a non-root process gets. `tools/e2e.py netperms`
 * assembles this file with `gcc -static -nostdlib`, injects the ELF into a
 * pagh-owned ext2 volume with debugfs, and runs it under the kernel's own
 * Linux-compat path with a NIC attached (the stack must be initialized for a
 * bind to reserve a port). Every result below is a real ring-3 syscall result.
 *
 * No libc: raw syscalls only. Output is one line per run; the harness greps the
 * `NPROBE` tag.
 *
 * Modes (`lxrun /mnt/netprobe <mode>`):
 *   root — root session: bind :80 and :8080 succeed, a second bind of the same
 *          port is EADDRINUSE, listen succeeds, SOCK_RAW is EOPNOTSUPP (the
 *          capability gate passes but no packet layer exists).
 *   drop — root drops to uid/gid 1000 first: bind :80 is EACCES, the *same*
 *          socket then binds :8080 (the refused bind left nothing reserved),
 *          listen succeeds, SOCK_RAW is EPERM.
 */

typedef unsigned long u64;
typedef long i64;

#define AF_INET 2
#define SOCK_STREAM 1
#define SOCK_RAW 3

#define EPROBE_NONE 0

/* ── raw syscalls ─────────────────────────────────────────────────────────── */

static inline i64 sc6(i64 nr, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) {
    i64 ret;
    register i64 r10 __asm__("r10") = d;
    register i64 r8 __asm__("r8") = e;
    register i64 r9 __asm__("r9") = f;
    __asm__ __volatile__("syscall"
                         : "=a"(ret)
                         : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9)
                         : "rcx", "r11", "memory");
    return ret;
}

static i64 sys_write(i64 fd, const void *buf, i64 n) {
    return sc6(1, fd, (i64)buf, n, 0, 0, 0);
}
static i64 sys_close(i64 fd) { return sc6(3, fd, 0, 0, 0, 0, 0); }
static i64 sys_socket(i64 domain, i64 ty, i64 proto) {
    return sc6(41, domain, ty, proto, 0, 0, 0);
}
static i64 sys_bind(i64 fd, const void *addr, i64 len) {
    return sc6(49, fd, (i64)addr, len, 0, 0, 0);
}
static i64 sys_listen(i64 fd, i64 backlog) { return sc6(50, fd, backlog, 0, 0, 0, 0); }
static i64 sys_getuid(void) { return sc6(102, 0, 0, 0, 0, 0, 0); }
static i64 sys_setuid(i64 uid) { return sc6(105, uid, 0, 0, 0, 0, 0); }
static i64 sys_setgid(i64 gid) { return sc6(106, gid, 0, 0, 0, 0, 0); }
static i64 sys_geteuid(void) { return sc6(107, 0, 0, 0, 0, 0, 0); }

/* ── freestanding output buffer: one deterministic line ───────────────────── */

static char out[512];
static long out_len;

static void put_c(char c) {
    if (out_len < (long)sizeof(out) - 1) {
        out[out_len++] = c;
    }
}
static void put_s(const char *s) {
    while (*s) {
        put_c(*s++);
    }
}
static void put_n(i64 v) {
    char tmp[24];
    int i = 0;
    u64 u;
    if (v < 0) {
        put_c('-');
        u = (u64)(-(v + 1)) + 1;
    } else {
        u = (u64)v;
    }
    if (u == 0) {
        put_c('0');
        return;
    }
    while (u && i < (int)sizeof(tmp)) {
        tmp[i++] = (char)('0' + (u % 10));
        u /= 10;
    }
    while (i > 0) {
        put_c(tmp[--i]);
    }
}
static void flush_out(void) {
    long off = 0;
    while (off < out_len) {
        i64 n = sys_write(1, out + off, out_len - off);
        if (n <= 0) {
            break;
        }
        off += n;
    }
    out_len = 0;
}

/* ── sockaddr_in + syscall outcomes ───────────────────────────────────────── */

struct sockaddr_in {
    unsigned short family;
    unsigned short port_be;
    unsigned int addr_be;
    unsigned char zero[8];
};

/* bind(2) outcome: 0 on success, else +errno. */
static i64 bind_port(i64 fd, unsigned short port_host) {
    struct sockaddr_in sa;
    unsigned char *p = (unsigned char *)&sa;
    for (u64 i = 0; i < sizeof(sa); i++) {
        p[i] = 0; /* freestanding: no memset dependency */
    }
    sa.family = AF_INET;
    sa.port_be = (unsigned short)((port_host << 8) | (port_host >> 8));
    i64 r = sys_bind(fd, &sa, (i64)sizeof(sa));
    return r < 0 ? -r : 0;
}

static i64 listen_result(i64 fd) {
    i64 r = sys_listen(fd, 5);
    return r < 0 ? -r : 0;
}

/* socket(2) outcome for a raw socket: 0 if created, else +errno. */
static i64 raw_result(void) {
    i64 fd = sys_socket(AF_INET, SOCK_RAW, 0);
    if (fd < 0) {
        return -fd;
    }
    sys_close(fd);
    return 0;
}

/* ── entry point ──────────────────────────────────────────────────────────── */

static int st_eq(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == 0 && *b == 0;
}

int entry(long argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "root";

    put_s("NPROBE mode=");
    put_s(mode);

    if (st_eq(mode, "drop")) {
        i64 sg = sys_setgid(1000);
        i64 su = sys_setuid(1000);
        put_s(" setgid=");
        put_n(sg < 0 ? -sg : 0);
        put_s(" setuid=");
        put_n(su < 0 ? -su : 0);
    }

    put_s(" uid=");
    put_n(sys_getuid());
    put_s(" euid=");
    put_n(sys_geteuid());

    /* One socket tries the privileged port first. When it succeeds (root) the
     * :8080 bind uses a fresh socket, because Linux rejects a second bind on an
     * already-bound socket with EINVAL. When it fails (uid 1000) the SAME fd
     * binds :8080, which proves the refusal reserved nothing. */
    i64 fd = sys_socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        put_s(" socket=");
        put_n(-fd);
        put_c('\n');
        flush_out();
        return 1;
    }
    i64 bind80 = bind_port(fd, 80);
    i64 fdb = fd;
    if (bind80 == 0) {
        fdb = sys_socket(AF_INET, SOCK_STREAM, 0);
        if (fdb < 0) {
            fdb = fd;
        }
    }
    put_s(" bind80=");
    put_n(bind80);
    put_s(" bind8080=");
    put_n(bind_port(fdb, 8080));

    /* A separate socket on the same port: EADDRINUSE once it is reserved. */
    i64 fd2 = sys_socket(AF_INET, SOCK_STREAM, 0);
    i64 dup = fd2 < 0 ? -fd2 : bind_port(fd2, 8080);
    put_s(" dup8080=");
    put_n(dup);
    if (fd2 >= 0) {
        sys_close(fd2);
    }

    put_s(" listen8080=");
    put_n(listen_result(fdb));
    put_s(" raw=");
    put_n(raw_result());

    /* close(2) on a listener must release the reserved port; bind it again on a
     * fresh socket. This is also what a well-behaved server does before exit. */
    sys_close(fdb);
    if (fdb != fd) {
        sys_close(fd);
    }
    i64 fd3 = sys_socket(AF_INET, SOCK_STREAM, 0);
    i64 after = fd3 < 0 ? -fd3 : bind_port(fd3, 8080);
    if (fd3 >= 0) {
        sys_close(fd3);
    }
    put_s(" afterclose=");
    put_n(after);
    put_c('\n');
    flush_out();
    return 0;
}

/* The kernel builds a normal Linux stack for execve (argc at [rsp], argv after
 * it) but there is no libc crt here, so dispatch from a naked entry exactly as
 * `group_probe.c` does. */
__attribute__((naked, noreturn)) void _start(void) {
    __asm__ __volatile__("mov (%rsp), %rdi\n"
                         "lea 8(%rsp), %rsi\n"
                         "and $-16, %rsp\n"
                         "call entry\n"
                         "mov %eax, %edi\n"
                         "mov $60, %eax\n"
                         "syscall\n"
                         "hlt\n");
}