/* x86vfs_poc.c — raw-syscall ring-3 regressions for the Pagh-OS x86_64
 * VFS/ext2 security batch (PR after #70, version 2.22.1).
 *
 * One static ELF, no libc: every check is a direct syscall so the kernel sees
 * exactly the sequence the test intends and the only output channel is fd 1
 * (console == serial evidence). The selector is a single digit read from
 * `/mnt/sel`; the injected caller (`tools/security/poc/x86vfs_repro.py`) writes
 * that file per scenario.
 *
 * Fixed-kernel expectations (the vulnerable 2.21.0 contrast is in
 * tools/security/README.md and the PR body):
 *
 *   sel 1  F-x86v-1  lstat crafted symlink (i_blocks=8, i_size=0xFFFFFFF0):
 *                    returns cleanly with st_size=0 instead of a 4 GiB
 *                    allocation panic.
 *   sel 2  F-x86v-1  readlink the same inode: ENOENT (target unreadable),
 *                    not an allocation panic.
 *   sel 3  F-x86v-2  rename a file whose crafted i_size claims ~4 GiB:
 *                    ENOMEM (bounded copy), not a 4 GiB `Vec` in the kernel.
 *   sel 4  F-x86v-3  uid 1000 opens `/rootdir/secret` (0644) inside a
 *                    root-owned 0700 directory: EACCES.
 *   sel 5  F-x86v-4  unlink a fast/slow-confused symlink whose i_block[0] is
 *                    the live data block of `/w/keep`: statfs free-block count
 *                    must NOT change (the old code freed that block).
 *   sel 6  F-x86v-5  lookup inside a directory whose i_size claims ~4 GiB:
 *                    ENOENT immediately instead of a ~1M-block stall.
 *   sel 7  F-x86v-6  (found while fixing the batch) execve a crafted regular
 *                    file whose i_size claims ~4 GiB: ENOEXEC, no allocation.
 *   sel 8  sanity: valid fast symlink -> lstat/readlink/open still work.
 *   sel 9  sanity: normal create/write/rename/read and traversal still work.
 *
 * Build (range toolchain, like the audit's PoC):
 *   gcc -static -O2 -nostdlib -no-pie -e _start -o x86vfs_poc x86vfs_poc.c
 */
typedef unsigned long u64;
typedef long i64;
typedef unsigned int u32;

static i64 sc6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) {
    i64 r;
    register i64 r10 __asm__("r10") = d;
    register i64 r8 __asm__("r8") = e;
    register i64 r9 __asm__("r9") = f;
    __asm__ volatile("syscall"
                     : "=a"(r)
                     : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9)
                     : "rcx", "r11", "memory");
    return r;
}
static i64 sc(i64 n, i64 a, i64 b, i64 c) { return sc6(n, a, b, c, 0, 0, 0); }

static u64 xstrlen(const char *s) {
    u64 n = 0;
    while (s[n]) n++;
    return n;
}
static void xputs(const char *s) { sc(1, 1, (i64)s, (i64)xstrlen(s)); }
static void xputd(long v) {
    char b[24];
    int i = 23;
    b[i] = 0;
    int neg = v < 0;
    unsigned long u = neg ? (unsigned long)(-v) : (unsigned long)v;
    if (!u) b[--i] = '0';
    while (u) {
        b[--i] = (char)('0' + u % 10);
        u /= 10;
    }
    if (neg) b[--i] = '-';
    xputs(b + i);
}
static void report(const char *tag, long rc) {
    xputs("POC:");
    xputs(tag);
    xputs(" rc=");
    xputd(rc);
    xputs("\n");
}

#define SYS_read 0
#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_newfstatat 262
#define SYS_unlink 87
#define SYS_rename 82
#define SYS_setuid 105
#define SYS_setgid 106
#define SYS_getuid 102
#define SYS_geteuid 107
#define SYS_readlink 89
#define SYS_statfs 137
#define SYS_execve 59
#define SYS_exit 60

#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR 2
#define O_CREAT 0100
#define AT_SYMLINK_NOFOLLOW 0x100

static int open_rd(const char *p) { return (int)sc(SYS_open, (i64)p, O_RDONLY, 0); }
static long read_fd(int fd, char *buf, long cap) {
    long n = (long)sc(SYS_read, fd, (i64)buf, cap - 1);
    if (n >= 0) buf[n] = 0;
    return n;
}
static long read_file(const char *p, char *buf, long cap) {
    int fd = open_rd(p);
    if (fd < 0) return fd;
    long n = read_fd(fd, buf, cap);
    sc(SYS_close, fd, 0, 0);
    return n;
}
static void drop_privs(void) {
    sc(SYS_setgid, 1000, 0, 0);
    sc(SYS_setuid, 1000, 0, 0);
}

/* x86_64 `struct stat`: st_size at offset 48, st_mode at 24. */
static char statbuf[256];

static int sel;

static void t1_lstat_crafted_symlink(void) {
    xputs("T1: lstat /mnt/evil_link\n");
    long rc = sc6(SYS_newfstatat, -100, (i64)"/mnt/evil_link", (i64)statbuf,
                  AT_SYMLINK_NOFOLLOW, 0, 0);
    report("newfstatat-noFollow", rc);
    if (rc == 0) {
        xputs("T1: st_size=");
        xputd(*(long *)(statbuf + 48));
        xputs("\n");
    }
}

static void t2_readlink_crafted_symlink(void) {
    static char buf[256];
    xputs("T2: readlink /mnt/evil_link\n");
    long rc = sc(SYS_readlink, (i64)"/mnt/evil_link", (i64)buf, 255);
    report("readlink", rc);
}

static void t3_rename_huge(void) {
    xputs("T3: rename /mnt/w/huge -> /mnt/w/huge2\n");
    long rc = sc(SYS_rename, (i64)"/mnt/w/huge", (i64)"/mnt/w/huge2", 0);
    report("rename-huge", rc);
}

static void t4_search_permission(void) {
    static char buf[64];
    xputs("T4: open /mnt/rootdir/secret as uid ");
    xputd((long)sc(SYS_getuid, 0, 0, 0));
    xputs("\n");
    long rc = read_file("/mnt/rootdir/secret", buf, sizeof buf);
    report("open-rootdir-secret", rc);
    if (rc >= 0) {
        xputs("T4: content=");
        xputs(buf);
    }
}

static void t5_unlink_fast_slow(void) {
    /* 120-byte `struct statfs`: f_bfree is a u64 at offset 24. */
    static char before[128], after[128];
    xputs("T5: statfs before / unlink /mnt/w/evil4 / statfs after\n");
    long rc0 = sc(SYS_statfs, (i64)"/mnt/w", (i64)before, 0);
    long rc1 = sc(SYS_unlink, (i64)"/mnt/w/evil4", 0, 0);
    long rc2 = sc(SYS_statfs, (i64)"/mnt/w", (i64)after, 0);
    if (rc0 == 0 && rc2 == 0) {
        long delta = (long)(*(u64 *)(after + 24) - *(u64 *)(before + 24));
        xputs("T5: bfree_before=");
        xputd((long)*(u64 *)(before + 24));
        xputs(" bfree_after=");
        xputd((long)*(u64 *)(after + 24));
        xputs(" delta=");
        xputd(delta);
        xputs("\n");
        report("unlink-evil4", rc1);
        report("free-blocks-delta", delta);
    } else {
        report("statfs", rc0 != 0 ? rc0 : rc2);
        report("unlink-evil4", rc1);
    }
}

static void t6_huge_directory(void) {
    xputs("T6: open /mnt/w/bigdir/x\n");
    int fd = open_rd("/mnt/w/bigdir/x");
    report("open-in-bigdir", (long)fd);
    if (fd >= 0) sc(SYS_close, fd, 0, 0);
}

static void t7_exec_huge_file(void) {
    xputs("T7: execve /mnt/w/bigfile\n");
    long rc = sc6(SYS_execve, (i64)"/mnt/w/bigfile", 0, 0, 0, 0, 0);
    report("execve-bigfile", rc);
}

static void t8_valid_symlink(void) {
    static char buf[128];
    xputs("T8: valid symlink /mnt/w/goodlink\n");
    long rc = sc6(SYS_newfstatat, -100, (i64)"/mnt/w/goodlink", (i64)statbuf,
                  AT_SYMLINK_NOFOLLOW, 0, 0);
    report("lstat-goodlink", rc);
    if (rc == 0) {
        xputs("T8: st_size=");
        xputd(*(long *)(statbuf + 48));
        xputs("\n");
    }
    long rl = sc(SYS_readlink, (i64)"/mnt/w/goodlink", (i64)buf, 127);
    report("readlink-goodlink", rl);
    if (rl > 0) {
        buf[rl] = 0;
        xputs("T8: target=");
        xputs(buf);
        xputs("\n");
    }
    long n = read_file("/mnt/w/goodlink", buf, sizeof buf);
    report("open-through-goodlink", n);
    if (n > 0) {
        xputs("T8: content=");
        xputs(buf);
    }
}

static void t9_normal_io(void) {
    static char buf[128];
    xputs("T9: normal create/write/rename/read in /mnt/w\n");
    int fd = (int)sc(SYS_open, (i64)"/mnt/w/small", O_RDWR | O_CREAT, 0644);
    report("create-small", (long)fd);
    if (fd >= 0) {
        long w = sc(SYS_write, fd, (i64)"SMALL-DATA", 10);
        report("write-small", w);
        sc(SYS_close, fd, 0, 0);
    }
    long rn = sc(SYS_rename, (i64)"/mnt/w/small", (i64)"/mnt/w/small2", 0);
    report("rename-small", rn);
    long n = read_file("/mnt/w/small2", buf, sizeof buf);
    report("read-renamed", n);
    if (n > 0) {
        xputs("T9: content=");
        xputs(buf);
        xputs("\n");
    }
    long rm = sc(SYS_unlink, (i64)"/mnt/w/small2", 0, 0);
    report("unlink-small", rm);
    /* Traversal through an ordinary (0755) subdirectory still works. */
    long sub = read_file("/mnt/w/sub/normal", buf, sizeof buf);
    report("read-sub-normal", sub);
    if (sub > 0) {
        xputs("T9: content=");
        xputs(buf);
    }
}

void _start(void) {
    static char selbuf[16];
    long n = read_file("/mnt/sel", selbuf, sizeof selbuf);
    sel = (n > 0 && selbuf[0] >= '0' && selbuf[0] <= '9') ? selbuf[0] - '0' : 0;

    drop_privs();
    xputs("POC-started uid=");
    xputd((long)sc(SYS_getuid, 0, 0, 0));
    xputs(" euid=");
    xputd((long)sc(SYS_geteuid, 0, 0, 0));
    xputs(" sel=");
    xputd(sel);
    xputs("\n");

    switch (sel) {
    case 1: t1_lstat_crafted_symlink(); break;
    case 2: t2_readlink_crafted_symlink(); break;
    case 3: t3_rename_huge(); break;
    case 4: t4_search_permission(); break;
    case 5: t5_unlink_fast_slow(); break;
    case 6: t6_huge_directory(); break;
    case 7: t7_exec_huge_file(); break;
    case 8: t8_valid_symlink(); break;
    case 9: t9_normal_io(); break;
    default: xputs("POC: unknown selector\n"); break;
    }
    xputs("POC-done\n");
    sc(SYS_exit, 0, 0, 0);
    for (;;) {
    }
}