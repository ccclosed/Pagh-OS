#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile int child_tid;
static volatile int ran;
static unsigned char stack[64 * 1024];
static int child(void *arg) {
    (void)arg;
    if (getpid() != *(pid_t *)&ran) _exit(91);
    ran = 2;
    return 0;
}
int main(void) {
    ran = (int)getpid();
    unsigned long flags = CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM|CLONE_CHILD_SETTID|CLONE_CHILD_CLEARTID;
    long tid = clone(child, stack + sizeof stack, flags, 0, 0, 0, (int *)&child_tid);
    if (tid < 0) { perror("clone"); return 1; }
    while (child_tid != 0 || ran != 2) syscall(SYS_futex, (int *)&child_tid, FUTEX_WAIT, child_tid, 0, 0, 0);
    puts("clone-thread: OK"); return 0;
}
