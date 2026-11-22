/*
 * usb_probe.c — freestanding x86_64 Linux probe for USB mass storage S1.
 *
 * WHY THIS EXISTS. `mount(2)`/`umount2(2)`, the exclusive `/dev/uevent` reader
 * and mounted-FS I/O are ring-3 contracts. The in-QEMU selftest runs in ring 0,
 * so it can check the pure layouts but cannot make a real syscall or observe
 * EBUSY/EIO. This probe passes every argument through the kernel's Linux-compat
 * path exactly as a compositor would.
 *
 * No libc: raw syscalls (so it cannot depend on the host glibc). Output is
 * line-oriented with the `UPROBE` tag; tools/e2e.py `usb-storage` greps it.
 *
 * Modes:
 *   mount <src> <tgt>          mount(2) nr 165, fstype "ext2", flags 0
 *   umount <tgt>               umount2(2) nr 166, flags 0
 *   umountf <tgt>              umount2(2) with MNT_FORCE (1)
 *   hold <file> <secs>         open, sleep (harness unplugs), read: open fd -> EIO
 *   busy <file> <tgt>          hold an fd and prove umount2 == EBUSY, then close
 *                              (the caller does the eventual clean umount)
 *   uevent <n>                 read up to n records from /dev/uevent (O_NONBLOCK),
 *                              then PUEV_IOC_SNAPSHOT (0x5501) and read again
 *   write <path> <text>        open(O_WRONLY|O_CREAT|O_TRUNC,0644)+write
 *   read <path>                open(O_RDONLY)+read(256)+print
 *   statfs <path>              statfs(2) nr 137
 *   stat <path>                stat(2) nr 4, print mode
 */

typedef unsigned long u64;
typedef long i64;
typedef unsigned int u32;
typedef unsigned short u16;

#define O_RDONLY 0
#define O_WRONLY 1
#define O_CREAT 0100
#define O_TRUNC 01000
#define O_NONBLOCK 04000

#define EINVAL 22
#define EBUSY 16
#define EIO 5
#define ENODEV 19
#define EPERM 1

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
static i64 sys_write(i64 fd, const void *buf, i64 n) { return sc6(1, fd, (i64)buf, n, 0, 0, 0); }
static i64 sys_open(const char *path, i64 flags, i64 mode) { return sc6(2, (i64)path, flags, mode, 0, 0, 0); }
static i64 sys_close(i64 fd) { return sc6(3, fd, 0, 0, 0, 0, 0); }
static i64 sys_stat(const char *path, void *st) { return sc6(4, (i64)path, (i64)st, 0, 0, 0, 0); }
static i64 sys_poll(void *fds, i64 n, i64 timeout) { return sc6(7, (i64)fds, n, timeout, 0, 0, 0); }
static i64 sys_ioctl(i64 fd, i64 req, i64 arg) { return sc6(16, fd, req, arg, 0, 0, 0); }
static i64 sys_statfs(const char *path, void *buf) { return sc6(137, (i64)path, (i64)buf, 0, 0, 0, 0); }
static i64 sys_nanosleep(const void *req, void *rem) { return sc6(35, (i64)req, (i64)rem, 0, 0, 0, 0); }
static i64 sys_mount(const char *src, const char *tgt, const char *fs, u64 flags, u64 data) {
    return sc6(165, (i64)src, (i64)tgt, (i64)fs, flags, data, 0);
}
static i64 sys_umount2(const char *tgt, i64 flags) { return sc6(166, (i64)tgt, flags, 0, 0, 0, 0); }
static void sys_exit(i64 code) { sc6(60, code, 0, 0, 0, 0, 0); for (;;) {} }

/* ── freestanding helpers ─────────────────────────────────────────────────── */

static u64 strlen(const char *s) { u64 n = 0; while (s[n]) n++; return n; }
static int streq(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return *a == *b;
}
static void puts(const char *s) { sys_write(1, s, strlen(s)); }
static void put_i64(i64 v) {
    char buf[32]; int i = 0; u64 u;
    if (v < 0) { sys_write(1, "-", 1); u = (u64)(-v); } else u = (u64)v;
    if (u == 0) { sys_write(1, "0", 1); return; }
    while (u) { buf[i++] = '0' + (u % 10); u /= 10; }
    while (i) { sys_write(1, &buf[--i], 1); }
}
static void put_hex2(u32 v) {
    const char *d = "0123456789abcdef";
    char b[2] = { d[(v >> 4) & 15], d[v & 15] };
    sys_write(1, b, 2);
}
static void put_uuid(const unsigned char *u) {
    int i;
    for (i = 0; i < 16; i++) { put_hex2(u[i]); }
}
static void put_field(const char *tag, const unsigned char *p, int n) {
    int i;
    puts(tag);
    puts("=");
    for (i = 0; i < n && p[i]; i++) sys_write(1, &p[i], 1);
}
static u16 le16(const unsigned char *p) { return (u16)p[0] | ((u16)p[1] << 8); }
static u32 le32(const unsigned char *p) {
    return (u32)p[0] | ((u32)p[1] << 8) | ((u32)p[2] << 16) | ((u32)p[3] << 24);
}
static u64 le64(const unsigned char *p) { return (u64)le32(p) | ((u64)le32(p + 4) << 32); }

/* ── /dev/uevent ──────────────────────────────────────────────────────────── */

static int dump_uevent(int want) {
    unsigned char rec[144];
    int got = 0;
    i64 fd = sys_open("/dev/uevent", O_RDONLY | O_NONBLOCK, 0);
    if (fd < 0) {
        puts("UPROBE uevent open rc="); put_i64(fd); puts("\n");
        return 1;
    }
    puts("UPROBE uevent open rc=0\n");
    while (got < want) {
        i64 n = sys_read(fd, rec, sizeof(rec));
        if (n == 144) {
            u32 magic = le32(rec), seq = le32(rec + 12);
            u16 ver = le16(rec + 4);
            u64 sectors = le64(rec + 16);
            puts("UPROBE UE");
            puts(" magic="); put_i64(magic);
            puts(" ver="); put_i64(ver);
            puts(" kind="); put_i64(rec[6]);
            puts(" flags="); put_i64(rec[7]);
            puts(" fs="); put_i64(rec[8]);
            puts(" err="); put_i64(rec[9]);
            puts(" seq="); put_i64(seq);
            puts(" sectors="); put_i64((i64)sectors);
            put_field(" dev", rec + 28, 16);
            put_field(" mount", rec + 44, 16);
            put_field(" vendor", rec + 60, 16);
            put_field(" product", rec + 76, 32);
            put_field(" label", rec + 108, 16);
            puts(" uuid="); put_uuid(rec + 128);
            puts("\n");
            got++;
        } else if (n < 0) {
            puts("UPROBE uevent read rc="); put_i64(n); puts("\n");
            break;
        } else {
            puts("UPROBE uevent empty rc="); put_i64(n); puts("\n");
            break;
        }
    }
    /* re-seed: the snapshot is idempotent by `dev`, so read a few ADD records. */
    i64 rc = sys_ioctl(fd, 0x5501, 0);
    puts("UPROBE uevent snapshot rc="); put_i64(rc); puts("\n");
    for (int i = 0; i < 3; i++) {
        i64 n = sys_read(fd, rec, sizeof(rec));
        if (n == 144) {
            puts("UPROBE UES kind="); put_i64(rec[6]);
            puts(" seq="); put_i64(le32(rec + 12));
            put_field(" dev", rec + 28, 16);
            puts("\n");
        } else {
            break;
        }
    }
    (void)sys_close(fd);
    return 0;
}

/* ── entry ────────────────────────────────────────────────────────────────── */

static int main(i64 argc, char **argv) {
    if (argc < 2) { puts("UPROBE usage\n"); return 2; }
    if (streq(argv[1], "mount") && argc >= 4) {
        i64 rc = sys_mount(argv[2], argv[3], "ext2", 0, 0);
        puts("UPROBE mount rc="); put_i64(rc); puts("\n");
    } else if (streq(argv[1], "umount") && argc >= 3) {
        i64 rc = sys_umount2(argv[2], 0);
        puts("UPROBE umount rc="); put_i64(rc); puts("\n");
    } else if (streq(argv[1], "umountf") && argc >= 3) {
        i64 rc = sys_umount2(argv[2], 1);
        puts("UPROBE umountf rc="); put_i64(rc); puts("\n");
    } else if (streq(argv[1], "uevent")) {
        i64 want = argc >= 3 ? 0 : 8;
        if (argc >= 3) { i64 v = 0; const char *s = argv[2]; while (*s >= '0' && *s <= '9') v = v * 10 + (*s++ - '0'); want = v; }
        return dump_uevent((int)want);
    } else if (streq(argv[1], "write") && argc >= 4) {
        i64 fd = sys_open(argv[2], O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd < 0) { puts("UPROBE write open rc="); put_i64(fd); puts("\n"); return 1; }
        i64 n = sys_write(fd, argv[3], strlen(argv[3]));
        sys_close(fd);
        puts("UPROBE write rc="); put_i64(n); puts("\n");
    } else if (streq(argv[1], "read") && argc >= 3) {
        char buf[300];
        i64 fd = sys_open(argv[2], O_RDONLY, 0);
        if (fd < 0) { puts("UPROBE read open rc="); put_i64(fd); puts("\n"); return 1; }
        i64 n = sys_read(fd, buf, sizeof(buf) - 1);
        sys_close(fd);
        if (n < 0) { puts("UPROBE read rc="); put_i64(n); puts("\n"); return 1; }
        buf[n] = 0;
        puts("UPROBE read rc="); put_i64(n); puts(" data="); puts(buf); puts("\n");
    } else if (streq(argv[1], "hold") && argc >= 4) {
        /* Open a file, sleep while the harness unplugs the device, then read:
         * the already-open descriptor must get EIO, not the old contents. */
        i64 secs = 0;
        for (const char *s = argv[3]; *s >= '0' && *s <= '9'; s++) secs = secs * 10 + (*s - '0');
        i64 fd = sys_open(argv[2], O_RDONLY, 0);
        puts("UPROBE hold open rc="); put_i64(fd); puts("\n");
        if (fd < 0) return 1;
        struct { i64 sec; i64 nsec; } ts;
        ts.sec = secs; ts.nsec = 0;
        (void)sys_nanosleep(&ts, 0);
        char buf[64];
        i64 n = sys_read(fd, buf, sizeof(buf));
        puts("UPROBE hold read rc="); put_i64(n); puts("\n");
        sys_close(fd);
    } else if (streq(argv[1], "busy") && argc >= 4) {
        /* Open a file under the mount, then try to unmount: must be EBUSY.
         * Closing the descriptor makes the same call succeed. */
        i64 fd = sys_open(argv[2], O_RDONLY, 0);
        puts("UPROBE busy open rc="); put_i64(fd); puts("\n");
        i64 rc = sys_umount2(argv[3], 0);
        puts("UPROBE busy umount rc="); put_i64(rc); puts("\n");
        if (fd >= 0) sys_close(fd);
        puts("UPROBE busy closed\n");
    } else if (streq(argv[1], "statfs") && argc >= 3) {
        unsigned char st[120];
        i64 rc = sys_statfs(argv[2], st);
        puts("UPROBE statfs rc="); put_i64(rc);
        if (rc == 0) { puts(" blocks="); put_i64((i64)le64(st)); puts(" free="); put_i64((i64)le64(st + 16)); }
        puts("\n");
    } else if (streq(argv[1], "stat") && argc >= 3) {
        unsigned char st[160];
        i64 rc = sys_stat(argv[2], st);
        puts("UPROBE stat rc="); put_i64(rc);
        if (rc == 0) { u32 mode = le32(st + 24); puts(" type="); put_i64((mode >> 12) & 15); puts(" perm="); put_i64(mode & 07777); }
        puts("\n");
    } else {
        puts("UPROBE unknown mode\n");
        return 2;
    }
    return 0;
}

void _start(void) {
    i64 *sp;
    __asm__ __volatile__("mov %%rsp, %0" : "=r"(sp));
    i64 argc = sp[0];
    char **argv = (char **)&sp[1];
    sys_exit(main(argc, argv));
}
