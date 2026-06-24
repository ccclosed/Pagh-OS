// boot.rs — Kernel boot orchestrator
// 64-bit x86_64 Limine kernel in Rust (#![no_std])
//
// This module owns the ordered, fallible kernel initialization sequence that
// previously lived inline in `lib.rs::_start` (Requirement 1). `_start` now
// simply delegates to `boot::start()`.
//
// Structure (Requirement 1):
//  * 1.1 — initialization runs as an explicit ordered sequence of init steps,
//          modeled as one private `fn` per phase invoked in order by `start()`.
//  * 1.3 — each completed step emits a single concise `info!` line; verbose
//          detail stays at `debug!`.
//  * 1.4 — a missing required Limine response or a failed step funnels through
//          the centralized `fatal()` helper, which logs one `error!` line
//          naming the failed step and enters `arch::cpu::halt_loop()`.
//
// NOTE: behavior is intentionally kept identical to the previous inline
// `_start` for now. The inline LAPIC/IOAPIC MMIO mapping stays here (it moves
// to the APIC/memory owners in task 7). `KERNEL_SIZE` is computed precisely
// from the kernel image extent (linker symbols) in `read_limine_responses`.

use core::sync::atomic::Ordering;

use crate::{debug, error, info, warn};
use crate::{arch, drivers, memory, net, shell, task, vfs};
use crate::{BASE_REVISION, HHDM_REQUEST, KERNEL_ADDR_REQUEST, MEMMAP_REQUEST};
use crate::{HHDM_OFFSET, KERNEL_BASE, KERNEL_SIZE};

/// Centralized fatal-boot handler (Requirement 1.4).
///
/// Logs exactly one `error!` line naming the failed step, then parks the CPU
/// forever via the safe `arch::cpu` halt primitive. Never returns.
fn fatal(step: &str) -> ! {
    error!("boot: {} failed", step);
    arch::cpu::halt_loop();
}

/// Run the ordered kernel initialization sequence, then enter the idle loop.
///
/// Each call below is one init step (Requirement 1.1). Steps that depend on a
/// Limine response read it from the request statics in `lib.rs`; missing
/// responses route through [`fatal`] (Requirement 1.4).
pub fn start() -> ! {
    // Step 0: enable SSE before anything else. The kernel and Rust's
    // `extern "x86-interrupt"` handlers emit SSE (`movaps`) instructions, but
    // Limine hands off with SSE not OS-enabled, so this must happen before any
    // FP/SSE use and before interrupts are enabled. See `arch::cpu::enable_sse`.
    arch::cpu::enable_sse();

    init_serial();
    check_base_revision();
    read_limine_responses();
    init_descriptor_tables();
    init_syscalls();
    init_pmm();
    init_vmm();
    init_heap();
    init_apic();
    init_drivers();
    init_virtio();
    init_scheduler();
    init_vfs();
    init_fs();
    init_net();

    // Hand off to the run phase (spawns the shell, enables interrupts, idles).
    kernel_main();
}

/// Step 1: bring up the COM1 serial port (the primary log sink).
fn init_serial() {
    drivers::serial::init();
    info!("serial");
}

/// Step 2: verify Limine handed us a base revision we support.
fn check_base_revision() {
    if !BASE_REVISION.is_supported() {
        fatal("base revision");
    }
    info!("base revision");
}

/// Step 3: read the required Limine responses (HHDM offset + kernel address)
/// and store them into the global address cells in `lib.rs`.
fn read_limine_responses() {
    match HHDM_REQUEST.response() {
        Some(hhdm) => HHDM_OFFSET.store(hhdm.offset, Ordering::Relaxed),
        None => fatal("hhdm"),
    }

    match KERNEL_ADDR_REQUEST.response() {
        Some(kaddr) => {
            KERNEL_BASE.store(kaddr.virtual_base, Ordering::Relaxed);
            // KERNEL_SIZE is the kernel image extent, computed precisely from
            // the linker symbols (`__kernel_end - __kernel_start`) rather than
            // a hardcoded guess. KERNEL_BASE stays the Limine virtual base.
            KERNEL_SIZE.store(memory::layout::kernel_size(), Ordering::Relaxed);
        }
        None => fatal("kernel address"),
    }

    info!("limine responses");
}

/// Step 4: load the GDT (with TSS/IST) and install the IDT.
fn init_descriptor_tables() {
    arch::x86_64::gdt::init();
    arch::x86_64::idt::init();
    info!("gdt + idt");
}

/// Step 4.5: enable the SYSCALL/SYSRET fast system-call interface.
///
/// Must run after `init_descriptor_tables` because `syscall::init` reads the
/// GDT selectors (`Selectors::user_code()` / `kernel_code()`) to program STAR.
fn init_syscalls() {
    arch::x86_64::syscall::init();
    info!("syscalls");
}

/// Step 5: initialize the physical frame allocator from the Limine memmap.
fn init_pmm() {
    match MEMMAP_REQUEST.response() {
        Some(memmap) => memory::pmm::init(memmap),
        None => fatal("memmap"),
    }
    info!("pmm");
}

/// Step 6: initialize the virtual memory manager over the HHDM.
fn init_vmm() {
    let hhdm_offset = HHDM_OFFSET.load(Ordering::Relaxed);
    memory::vmm::init(hhdm_offset);
    info!("vmm");
}

/// Step 7: initialize the kernel heap / global allocator.
fn init_heap() {
    memory::heap::init();
    info!("heap");
}

/// Step 8: initialize the APIC and route the keyboard IRQ.
///
/// The APIC owns its own LAPIC/IOAPIC MMIO mapping (Requirement 7.4):
/// `apic::init()` establishes those mappings internally via `vmm::map_mmio`
/// before touching any registers, so no MMIO mapping happens inline here.
fn init_apic() {
    arch::x86_64::apic::init();

    // Route IRQ1 (keyboard) -> vector 33 and register its handler.
    arch::x86_64::apic::route_irq(1, 33);
    arch::x86_64::apic::register_irq(33, drivers::ps2_kbd::irq_handler);

    // Route IRQ12 (PS/2 mouse) -> vector 44 and register its handler. The
    // mouse device itself is enabled later in `init_drivers` (after the
    // framebuffer is up, so it knows the screen bounds); routing the IOAPIC
    // redirection entry early is harmless because no packets stream until the
    // device's data reporting is turned on.
    arch::x86_64::apic::route_irq(12, 44);
    arch::x86_64::apic::register_irq(44, drivers::ps2_mouse::irq_handler);

    info!("apic");
}

/// Step 9: initialize the device manager (PS/2 keyboard + framebuffer).
///
/// The framebuffer needs no MMIO mapping of its own: Limine maps the
/// framebuffer into the higher half and hands us the already-mapped virtual
/// address (`fb.address()`), so the driver uses that mapping directly.
fn init_drivers() {
    drivers::init();
    info!("drivers");
}

/// Step 9.5: discover and attach virtio devices (PCI enumeration + drivers).
///
/// Runs after `init_drivers` so the heap is up (PCI `enumerate()` allocates a
/// `Vec`). Attaches the virtio-blk disk as a `BlockDevice` ("virtio-blk0"). If
/// no virtio device of a given kind is present, the corresponding attach logs a
/// warning and no-ops so boot is always preserved (R17.4).
///
/// TODO(task 6): also attach the virtio-net NIC here (smoltcp bring-up).
fn init_virtio() {
    let devs = drivers::pci::enumerate();
    drivers::virtio::blk::init_blk(&devs);
    info!("virtio");
}

/// Step 10: initialize the preemptive scheduler.
fn init_scheduler() {
    task::scheduler::init();
    info!("scheduler");
}

/// Step 11: initialize the virtual filesystem.
fn init_vfs() {
    debug!("About to call vfs::init()...");
    vfs::init();
    debug!("vfs::init() returned");
    info!("vfs");
}

/// Step 11.5: bring the ext2 filesystem up on the real virtio-blk disk and
/// mount it at `/mnt` (Task 5.1, R5.6/R9.1/R9.2).
///
/// Ordering note: this runs *after* `init_vfs` (not inside `init_virtio`)
/// because `vfs::mount_at` needs the VFS root to already exist. The block
/// device itself was registered earlier in `init_virtio`; heap/pmm/vmm are all
/// up, and interrupts are still disabled during boot init, so the format/mount
/// disk I/O and allocations run on a single, non-preempted path.
///
/// If no virtio-blk device is present this logs a warning and continues booting
/// (R17.4). On first boot (no valid superblock) the disk is formatted then
/// mounted. After a successful mount a one-shot self-demo writes a file under
/// `/mnt` and reads it back, proving the real-disk journaled write+read path
/// end-to-end on a headless boot.
fn init_fs() {
    use crate::fs::ext2::Ext2Fs;

    let blk = match drivers::get_block("virtio-blk0") {
        Some(b) => b,
        None => {
            warn!("fs: no virtio-blk device; /mnt not mounted");
            info!("fs (no disk)");
            return;
        }
    };

    // First boot: if there is no valid ext2 superblock yet, format the disk.
    if Ext2Fs::mount(blk.clone()).is_err() {
        info!("fs: no ext2 filesystem found, formatting disk...");
        if let Err(e) = Ext2Fs::format(blk.clone()) {
            error!("fs: format failed: {:?}; /mnt not mounted", e);
            return;
        }
    }

    match Ext2Fs::mount(blk) {
        Ok(root) => {
            if let Err(e) = vfs::mount_at("/mnt", root) {
                error!("fs: mount_at(/mnt) failed: {:?}", e);
                return;
            }
            info!("ext2 mounted at /mnt");
            fs_boot_demo();
        }
        Err(e) => warn!("ext2 mount failed: {:?}", e),
    }
    info!("fs");
}

/// Step 11.6: bring up networking (Task 6). Enumerates PCI, attaches the
/// virtio-net NIC, builds the smoltcp interface, and enables the UDP echo
/// service on port 7. Address acquisition (DHCP, static fallback) and packet
/// servicing run in the net thread spawned from `kernel_main` once interrupts
/// are enabled and the 100 Hz tick is advancing the network clock.
///
/// If no virtio-net device is present this logs a warning and continues booting
/// (R17.3). Networking is fully optional — boot is always preserved.
fn init_net() {
    match net::init() {
        Ok(()) => {
            net::udp_echo_enable(7);
            net::tcp_echo_listen(7);
            info!("net");
        }
        Err(e) => {
            warn!("net: no interface available ({:?}); networking disabled", e);
            info!("net (no nic)");
        }
    }
}

/// One-shot boot self-demo (Task 5.5): exercise the real-disk journaled write
/// and read-back path so a headless boot proves it works end-to-end. Writes a
/// known string to `/mnt/bootdemo.txt`, then re-reads it through a fresh VFS
/// lookup and logs PASS/FAIL. Non-fatal: any error is logged and boot proceeds.
fn fs_boot_demo() {
    use crate::vfs::{self, VfsError};

    const CONTENT: &[u8] = b"pagh-ext2-journaled-write-OK";

    let dir = match vfs::lookup_path("/mnt") {
        Ok(d) => d,
        Err(e) => {
            warn!("fs demo: /mnt lookup failed: {:?}", e);
            return;
        }
    };

    // Create or open the demo file (tolerate it already existing from a prior
    // boot, since the disk image persists across runs).
    let file = match dir.create_file("bootdemo.txt") {
        Ok(f) => f,
        Err(VfsError::AlreadyExists) => match dir.lookup("bootdemo.txt") {
            Ok(f) => f,
            Err(e) => {
                warn!("fs demo: open existing failed: {:?}", e);
                return;
            }
        },
        Err(e) => {
            warn!("fs demo: create_file failed: {:?}", e);
            return;
        }
    };

    match file.write(0, CONTENT) {
        Ok(n) if n == CONTENT.len() => {}
        Ok(n) => {
            warn!("fs demo: short write ({} of {} bytes)", n, CONTENT.len());
            return;
        }
        Err(e) => {
            warn!("fs demo: write failed: {:?}", e);
            return;
        }
    }
    dir.sync();

    // Read back through a fresh lookup to avoid any cached node state.
    let rfile = match vfs::lookup_path("/mnt/bootdemo.txt") {
        Ok(f) => f,
        Err(e) => {
            warn!("fs demo: read-back lookup failed: {:?}", e);
            return;
        }
    };
    let mut buf = [0u8; 64];
    match rfile.read(0, &mut buf) {
        Ok(n) if &buf[..n] == CONTENT => {
            info!("fs demo: /mnt/bootdemo.txt write+read round-trip PASS ({} bytes)", n);
        }
        Ok(n) => {
            error!("fs demo: read-back MISMATCH ({} bytes): FAIL", n);
        }
        Err(e) => warn!("fs demo: read-back failed: {:?}", e),
    }
}

/// Run phase: spawn the shell thread, enable interrupts, and become the idle
/// loop.
///
/// The old ~5M-cycle busy-spin debug block (used only to observe timer IRQs
/// during bringup) is removed per Requirement 1.5.
fn kernel_main() -> ! {
    debug!("Spawning shell thread...");
    task::scheduler::kernel_thread_spawn(shell_thread);
    debug!("Shell thread spawned (PID should be 1)");

    // Spawn the networking poll thread (Task 6, R13.4). It must start once
    // scheduling is running and interrupts are enabled (below), because the
    // smoltcp clock advances off the 100 Hz timer tick and DHCP/echo servicing
    // relies on the periodic poll. If no NIC was attached, `net::poll` is a
    // cheap no-op, so spawning unconditionally is harmless.
    task::scheduler::kernel_thread_spawn(net::net_thread);
    debug!("Net thread spawned");

    // Spawn the embedded ring-3 test user process (Requirements 13.1 / 13.4).
    //
    // This MUST happen while interrupts are still disabled: `create_user_process`
    // briefly installs the user CR3 to populate the user address space, and a
    // timer tick in that window would let the scheduler observe the foreign CR3.
    // It also programs TSS RSP0 for the ring-3 → ring-0 transition.
    //
    // The process performs a `SYS_WRITE` (observable on serial) then `SYS_EXIT`,
    // dropping itself from the scheduler rotation; the shell thread continues to
    // run and the prompt stays interactive.
    match task::process::spawn_test_user_process() {
        Ok(pid) => info!("user test process spawned (pid {})", pid),
        Err(e) => error!("user test process failed: {}", e),
    }

    arch::cpu::enable_interrupts();
    info!("interrupts enabled");

    // Main thread becomes the idle loop.
    arch::cpu::halt_loop()
}

/// The shell kernel thread: let the scheduler stabilize, then run the shell.
fn shell_thread() {
    // Give scheduler time to stabilize.
    for _ in 0..1_000_000 {
        core::hint::spin_loop();
    }

    shell::shell_main();
}
