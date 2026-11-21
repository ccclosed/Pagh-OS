/*
 * tools/termux_marker.c — append one line to a file, freestanding x86_64 ELF.
 *
 * The stage-4 terminal acceptance must prove that a command typed into the
 * window really executed. The kernel shell has no `>` redirection, so the
 * proof is a tiny program: `lxrun /mnt/marker /mnt/q2 CDAB` appends
 * "CDAB\n" to /mnt/q2. Two invocations leave two lines, which is how the
 * selection+Ctrl+Y paste case is distinguished from a single execution.
 *
 * No libc, no startup files: raw syscalls only, so a green run cannot be
 * explained by libc behaviour. (Same policy as tools/group_probe.c.)
 */
typedef unsigned long u64;

static long syscall4(long n, long a, long b, long c, long d) {
    long r;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall"
                     : "=a"(r)
                     : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10)
                     : "rcx", "r11", "memory");
    return r;
}

static long sys3(long n, long a, long b, long c) {
    return syscall4(n, a, b, c, 0);
}

static void write_str(const char *s) {
    long len = 0;
    while (s[len])
        len++;
    sys3(1 /* write */, 1, (long)s, len);
}

static void fail(long code) __attribute__((noreturn));
static void fail(long code) {
    sys3(60 /* exit */, code, 0, 0);
    for (;;) {
    }
}

void start_c(long *sp) __attribute__((noreturn));
void start_c(long *sp) {
    long argc = sp[0];
    char **argv = (char **)(sp + 1);
    if (argc < 3)
        fail(2); /* need: marker FILE TEXT... */

    /* O_WRONLY | O_CREAT, mode 0644 (O_APPEND is not implemented by this
       Linux layer, so the position is moved to the end explicitly). */
    long fd = syscall4(257 /* openat */, -100 /* AT_FDCWD */,
                       (long)argv[1], 1 | 0100, 0644);
    if (fd < 0)
        fail(3);
    if (sys3(8 /* lseek */, fd, 0, 2 /* SEEK_END */) < 0)
        fail(4);

    for (long i = 2; i < argc; i++) {
        write_str(argv[i]);
        if (i + 1 < argc)
            write_str(" ");
    }
    write_str("\n");
    sys3(3 /* close */, fd, 0, 0);
    sys3(60 /* exit */, 0, 0, 0);
    for (;;) {
    }
}

__attribute__((naked)) void _start(void) {
    __asm__ volatile("mov %rsp, %rdi\n\t"
                     "call start_c\n\t");
}