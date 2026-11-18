/*
 * group_probe.c — freestanding x86_64 Linux probe for the phase-1 group model.
 *
 * WHY THIS EXISTS. The in-QEMU `selftest` routines run in ring 0, so they can
 * check `Creds` and VFS metadata but cannot pass a real user `gid_t*` through
 * `getgroups(2)`/`setgroups(2)` or observe `open(2)` decisions as a non-root
 * process. `tools/e2e.py groups` assembles this file with `gcc -static -nostdlib`,
 * injects the ELF into a pagh-owned ext2 volume with debugfs, and runs it under
 * the kernel's own Linux-compat path. Every result below is therefore a real
 * syscall result from ring 3.
 *
 * No libc: raw syscalls only (so the probe cannot accidentally depend on the
 * host's glibc and cannot be confused with a package-managed binary). The output
 * is line-oriented and stable; the harness greps the `GPROBE`/`GSET` tags.
 *
 * Modes (`lxrun /mnt/grpprobe <mode>`):
 *   root      — root session: getgroups/setgroups contract (count, EINVAL,
 *               EFAULT, cap, clear), gid 0 allowed.
 *   drop      — root drops to uid 1000 with NO supplementary groups: group-owned
 *               file and devices must be EACCES, /dev/null still works.
 *   dropvideo — root sets supplementary video(44) then drops to uid 1000:
 *               root:video file and fb0 open, vtconsole0 (tty) does not. Proves
 *               the group class uses *any* supplementary gid, not egid.
 *   file      — runs as uid 1000 after `su oleg` (session groups tty/video/
 *               input): everything the groups promise opens, and setgroups is
 *               EPERM. Proves su -> lxrun credential inheritance end to end.
 */

typedef unsigned long u64;
typedef long i64;

#define O_RDONLY 0
#define O_RDWR 2

#define R_OK 4
#define W_OK 2

#define EPERM 1
#define EFAULT 14
#define EINVAL 22
#define EACCES 13

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

static i64 sys_read(i64 fd, void *buf, i64 n) { return sc6(0, fd, (i64)buf, n, 0, 0, 0); }
static i64 sys_write(i64 fd, const void *buf, i64 n) {
    return sc6(1, fd, (i64)buf, n, 0, 0, 0);
}
static i64 sys_open(const char *path, i64 flags, i64 mode) {
    return sc6(2, (i64)path, flags, mode, 0, 0, 0);
}
static i64 sys_close(i64 fd) { return sc6(3, fd, 0, 0, 0, 0, 0); }
static i64 sys_access(const char *path, i64 mode) {
    return sc6(21, (i64)path, mode, 0, 0, 0, 0);
}
static i64 sys_getuid(void) { return sc6(102, 0, 0, 0, 0, 0, 0); }
static i64 sys_getgid(void) { return sc6(104, 0, 0, 0, 0, 0, 0); }
static i64 sys_setuid(i64 uid) { return sc6(105, uid, 0, 0, 0, 0, 0); }
static i64 sys_setgid(i64 gid) { return sc6(106, gid, 0, 0, 0, 0, 0); }
static i64 sys_geteuid(void) { return sc6(107, 0, 0, 0, 0, 0, 0); }
static i64 sys_getegid(void) { return sc6(108, 0, 0, 0, 0, 0, 0); }
static i64 sys_getgroups(i64 size, unsigned int *list) {
    return sc6(115, size, (i64)list, 0, 0, 0, 0);
}
static i64 sys_setgroups(i64 size, const unsigned int *list) {
    return sc6(116, size, (i64)list, 0, 0, 0, 0);
}
/* ── freestanding string/memory helpers (no libc) ─────────────────────────── */

void *memcpy(void *dst, const void *src, u64 n) {
    unsigned char *d = dst;
    const unsigned char *s = src;
    while (n--) {
        *d++ = *s++;
    }
    return dst;
}
void *memset(void *dst, int c, u64 n) {
    unsigned char *d = dst;
    while (n--) {
        *d++ = (unsigned char)c;
    }
    return dst;
}

static int st_eq(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == 0 && *b == 0;
}

/* ── output buffer: one write per probe, deterministic lines ──────────────── */

static char out[1024];
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

/* ── helpers ──────────────────────────────────────────────────────────────── */

static void put_identity(void) {
    put_s(" uid=");
    put_n(sys_getuid());
    put_s(" euid=");
    put_n(sys_geteuid());
    put_s(" gid=");
    put_n(sys_getgid());
    put_s(" egid=");
    put_n(sys_getegid());
    put_s(" ngroups=");
    put_n(sys_getgroups(0, 0));
    put_s(" groups=");
    unsigned int gs[32];
    i64 n = sys_getgroups(32, gs);
    for (i64 i = 0; i < n && i < 32; i++) {
        if (i) {
            put_c(',');
        }
        put_n(gs[i]);
    }
    if (n == 0) {
        put_c('-');
    }
}

/* Open a path and print its numeric outcome: 0 = opened, else +errno. */
static long open_result(const char *path, i64 flags) {
    i64 fd = sys_open(path, flags, 0);
    if (fd >= 0) {
        sys_close(fd);
        return 0;
    }
    return -fd;
}

static void put_open(const char *name, const char *path, i64 flags) {
    put_c(' ');
    put_s(name);
    put_c('=');
    put_n(open_result(path, flags));
}

/*
 * `access(2)` rather than `open(2)` for /dev: opening /dev/fb0 acquires the
 * one-way console takeover (by design), which would silently stop the kernel
 * console flushing to the framebuffer for the rest of the run. access(2) is
 * exactly the permission question here, has no side effect, and goes through
 * the real-id projection that keeps the supplementary group set.
 */
static void put_access(const char *name, const char *path, i64 mode) {
    i64 r = sys_access(path, mode);
    put_c(' ');
    put_s(name);
    put_c('=');
    put_n(r < 0 ? -r : 0);
}

/* Root-only preparation read: prove /mnt/groupfile exists before dropping. */
static void put_file_read(const char *path) {
    i64 fd = sys_open(path, O_RDONLY, 0);
    if (fd < 0) {
        put_s(" read=");
        put_n(-fd);
        return;
    }
    char buf[64];
    i64 n = sys_read(fd, buf, sizeof(buf));
    sys_close(fd);
    put_s(" read=");
    put_n(n);
}

/* ── modes ────────────────────────────────────────────────────────────────── */

static int mode_root(void) {
    unsigned int many[33];
    for (unsigned i = 0; i < 33; i++) {
        many[i] = 7;
    }
    unsigned int gs[3] = {0, 44, 105};
    unsigned int got[3] = {0xFFFF, 0xFFFF, 0xFFFF};

    i64 initial = sys_getgroups(0, 0);
    i64 invalid33 = sys_setgroups(33, many);
    i64 set = sys_setgroups(3, gs);
    i64 count = sys_getgroups(0, 0);
    i64 listed = sys_getgroups(3, got);
    i64 small = sys_getgroups(2, got);
    i64 badptr = sys_getgroups(3, (unsigned int *)1);
    i64 cleared = sys_setgroups(0, 0);
    i64 after = sys_getgroups(0, 0);

    put_s("GPROBE mode=root");
    put_identity();
    put_s(" initial=");
    put_n(initial);
    put_s(" invalid33=");
    put_n(invalid33 < 0 ? -invalid33 : 0);
    put_s(" set=");
    put_n(set < 0 ? -set : 0);
    put_s(" count=");
    put_n(count);
    put_s(" listed=");
    put_n(listed);
    put_s(" vals=");
    put_n(got[0]);
    put_c(',');
    put_n(got[1]);
    put_c(',');
    put_n(got[2]);
    put_s(" small=");
    put_n(small < 0 ? -small : 0);
    put_s(" badptr=");
    put_n(badptr < 0 ? -badptr : 0);
    put_s(" cleared=");
    put_n(cleared < 0 ? -cleared : 0);
    put_s(" after=");
    put_n(after);
    put_c('\n');
    flush_out();
    return 0;
}

static int mode_drop(int with_video) {
    /* Precondition as root: the data file created by the shell must be readable. */
    i64 root_read = open_result("/mnt/groupfile", O_RDONLY);

    unsigned int gs[1] = {44};
    i64 set = 0;
    if (with_video) {
        set = sys_setgroups(1, gs);
    } else {
        set = sys_setgroups(0, (const unsigned int *)0);
    }
    i64 sg = sys_setgid(1000);
    i64 su = sys_setuid(1000);

    put_s(with_video ? "GPROBE mode=dropvideo" : "GPROBE mode=drop");
    put_identity();
    put_s(" rootfile=");
    put_n(root_read);
    put_s(" setgroups=");
    put_n(set < 0 ? -set : 0);
    put_s(" setgid=");
    put_n(sg < 0 ? -sg : 0);
    put_s(" setuid=");
    put_n(su < 0 ? -su : 0);
    put_open("file", "/mnt/groupfile", O_RDONLY);
    put_file_read("/mnt/groupfile");
    put_access("fb0", "/dev/fb0", R_OK | W_OK);
    put_access("event0", "/dev/input/event0", R_OK);
    put_access("vtconsole", "/dev/vtconsole0", R_OK);
    put_access("null", "/dev/null", R_OK | W_OK);
    /* Now unprivileged: setgroups must be EPERM before any pointer is read. */
    i64 again = sys_setgroups(1, gs);
    put_s(" setgroups_again=");
    put_n(again < 0 ? -again : 0);
    put_c('\n');
    flush_out();
    return 0;
}

static int mode_file(void) {
    put_s("GPROBE mode=file");
    put_identity();
    put_open("file", "/mnt/groupfile", O_RDONLY);
    put_file_read("/mnt/groupfile");
    put_access("fb0", "/dev/fb0", R_OK | W_OK);
    put_access("event0", "/dev/input/event0", R_OK);
    put_access("vtconsole", "/dev/vtconsole0", R_OK);
    put_access("vtinput", "/dev/vtinput", W_OK);
    put_access("null", "/dev/null", R_OK | W_OK);
    unsigned int gs[1] = {10};
    i64 again = sys_setgroups(1, gs);
    put_s(" setgroups=");
    put_n(again < 0 ? -again : 0);
    put_c('\n');
    flush_out();
    return 0;
}

int entry(long argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "root";
    if (st_eq(mode, "root")) {
        return mode_root();
    }
    if (st_eq(mode, "drop")) {
        return mode_drop(0);
    }
    if (st_eq(mode, "dropvideo")) {
        return mode_drop(1);
    }
    if (st_eq(mode, "file")) {
        return mode_file();
    }
    put_s("GPROBE mode=unknown argc=");
    put_n(argc);
    put_c('\n');
    flush_out();
    return 2;
}

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