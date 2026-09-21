//! TarnOS kernel entry point.
//!
//! This file wires up the Limine boot protocol requests and hands off to
//! the rest of the kernel.
#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod arch;
mod driver;
#[macro_use]
mod earlycon;
mod elf;
mod ipc;
mod lang_items;
mod memory;
mod milestone2_tests;
mod milestone3_tests;
mod milestone4_tests;
mod milestone5_tests;
mod milestone7_tests;
mod milestone8_tests;
mod milestone11_tests;
mod kitchen_sink_tests;
mod sync;
mod task;

use core::arch::asm;
use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, ModulesRequest,
    MpRequest, RsdpRequest, StackSizeRequest,
};
use limine::BaseRevision;

/// Requested explicitly rather than relying on Limine's unspecified
/// default: boot code and the process-creation smoke test both build
/// sizable stack values before they're moved onto the heap, and a small
/// default stack would risk overflowing into whatever memory follows it.
#[used]
#[link_section = ".requests"]
static STACK_SIZE_REQUEST: StackSizeRequest = StackSizeRequest::new(0x10000);

/// Tells Limine we speak base revision 3 — the revision this kernel
/// actually relies on (RSDP is a physical address only from revision 3
/// onward). Pinned to a specific number rather than the crate's
/// max-supported constant: if a future Limine release doesn't yet honor a
/// newer max, requesting it would make [`BaseRevision::is_supported`]
/// false even though everything this kernel needs is still available.
#[used]
#[link_section = ".requests"]
static BASE_REVISION: BaseRevision = BaseRevision::with_revision(3);

#[used]
#[link_section = ".requests_start"]
static REQUESTS_START: limine::RequestsStartMarker = limine::RequestsStartMarker::new();
#[used]
#[link_section = ".requests_end"]
static REQUESTS_END: limine::RequestsEndMarker = limine::RequestsEndMarker::new();

/// Higher-half direct map offset: the only fact needed to translate a
/// physical address into a kernel-accessible virtual one (`memory::virt`).
#[used]
#[link_section = ".requests"]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

/// Usable/reserved/ACPI/bootloader-reclaimable region list, seeds the
/// physical frame allocator (`memory::phys`).
#[used]
#[link_section = ".requests"]
static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

/// The kernel's own physical+virtual load address, so the frame allocator
/// can reserve the kernel image's own frames.
#[used]
#[link_section = ".requests"]
static EXECUTABLE_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

/// Loads `init`'s ELF binary as a boot module — no filesystem/VFS driver is
/// needed this milestone since Limine hands us the raw bytes directly.
#[used]
#[link_section = ".requests"]
static MODULES_REQUEST: ModulesRequest = ModulesRequest::new();

/// Captured but still unused: SMP bring-up ended up going through
/// Limine's own `MP_REQUEST` instead (see `arch::x86_64::smp`), which
/// hands over each core's LAPIC ID directly with no ACPI/MADT parsing
/// needed. Left in place in case a future need for the ACPI tables
/// themselves (not just per-core LAPIC IDs) comes up, so that wouldn't
/// require a boot-protocol change either.
#[used]
#[link_section = ".requests"]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

/// Requested defensively but unused this milestone: all output goes through
/// the UART. Graphical console is explicitly deferred.
#[used]
#[link_section = ".requests"]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

/// Enumerates every CPU core Limine found (including the BSP itself) and
/// gives a `bootstrap()` handshake for starting each additional one — see
/// `arch::x86_64::smp`. Flags `0`: plain MMIO xAPIC access, not x2APIC
/// (`limine::mp::MP_FLAG_X2APIC`) — the simpler option, sufficient for
/// this milestone's core counts.
#[used]
#[link_section = ".requests"]
static MP_REQUEST: MpRequest = MpRequest::new(0);

// Every `#[cfg(feature = "...-test")]` block below that spawns its own
// dummy process(es) ends by calling `task::scheduler::start()` (`-> !`,
// never returns) instead of falling through to the real boot sequence —
// exactly one test feature is ever enabled in any real build (each
// `xtask test-*` scenario builds with its own single, fixed feature; the
// default/CI `build` enables none of them), so whichever one block is
// actually compiled in genuinely never falls through to anything after
// it. Rustc has no way to know only one of ~14 mutually-exclusive
// `#[cfg]` blocks is ever live in a given build, so it (correctly, for
// what it can see) flags the code textually following each one's own
// diverging call as unreachable. Restructuring this into a single
// if/else chain would fix that at the cost of a much larger diff for a
// purely cosmetic warning; `#[allow(unreachable_code)]` says so
// directly instead. Only ever silences *this* fully-understood,
// structural warning -- everything below still gets a real compile
// error for an actual type/borrow/logic mistake, since none of those
// are `unreachable_code` lints.
#[allow(unreachable_code)]
#[no_mangle]
extern "C" fn _start() -> ! {
    assert!(
        BASE_REVISION.is_supported(),
        "unsupported Limine base revision"
    );

    earlyprintln!("TarnOS booting...");

    if let Some(resp) = MEMMAP_REQUEST.response() {
        earlyprintln!("[boot] memory map received ({} entries)", resp.entries().len());
    }
    if let Some(resp) = HHDM_REQUEST.response() {
        earlyprintln!("[boot] HHDM offset received ({:#x})", resp.offset);
    }
    if EXECUTABLE_ADDRESS_REQUEST.response().is_some() {
        earlyprintln!("[boot] kernel address received");
    }
    if let Some(resp) = MODULES_REQUEST.response() {
        earlyprintln!("[boot] boot modules received ({})", resp.modules().len());
    }
    if RSDP_REQUEST.response().is_some() {
        earlyprintln!("[boot] RSDP received");
    }
    if let Some(resp) = MP_REQUEST.response() {
        earlyprintln!(
            "[boot] MP info received ({} CPU(s), BSP LAPIC ID {})",
            resp.cpus().len(),
            resp.bsp_lapic_id
        );
    }

    // Runs before `arch::x86_64::init()` now (it didn't in earlier
    // milestones): the double-fault IST stack that GDT setup installs
    // needs a real, guard-paged mapping (see `arch::x86_64::gdt`), which
    // needs the frame allocator and page mapper this call brings up.
    // Nothing before this point touches the heap or paging beyond what
    // Limine already set up, so the reorder is free.
    let hhdm_offset = x86_64::VirtAddr::new(
        HHDM_REQUEST
            .response()
            .expect("Limine did not honor the HHDM request")
            .offset,
    );
    let memmap_entries = MEMMAP_REQUEST
        .response()
        .expect("Limine did not honor the memory map request")
        .entries();
    unsafe {
        memory::init(hhdm_offset, memmap_entries);
    }
    earlyprintln!("[boot] memory management initialized (frame allocator, page mapper, heap)");

    // Must run before any process's AddressSpace is created (none exist
    // yet at this point in boot) — see the doc comment on
    // `task::process`'s `KERNEL_STACKS_BASE` for why every possible
    // process's kernel stack has to be mapped into the shared kernel
    // half eagerly, right now, rather than lazily per-process later.
    task::process::init_kernel_stacks();

    // Same ordering requirement as the call just above, for the same
    // reason -- see `task::scheduler::map_guarded`'s doc comment
    // (docs/adr/0025). Also brings `task::scheduler`'s own state up in
    // the first place: nothing else in this module locks `SCHEDULER`
    // before this point.
    task::scheduler::init();

    // Same ordering requirement as both calls just above, for the same
    // reason -- see `task::process::init_trap_frames`'s own doc comment
    // (docs/adr/0026).
    task::process::init_trap_frames();

    // Same ordering requirement as every call above, for the same
    // reason -- see `task::scheduler::init_process_slots`'s own doc
    // comment (docs/adr/0027).
    task::scheduler::init_process_slots();

    // Every boot module besides "init" itself becomes a program
    // SYS_SPAWN can create a process from by name — there is no
    // filesystem yet, so this fixed, boot-time set is the only source a
    // running process has for a new process's code. See
    // docs/adr/0006-dynamic-process-creation-and-capability-transfer.md.
    // Populated here (needs the heap, so after `memory::init()`) rather
    // than down by the real boot sequence below, since milestone-2/3's
    // test-only boot branches (which run instead of, not before, that
    // real sequence) need it too — `spawn-boundary-test` exercises
    // SYS_SPAWN directly.
    let boot_modules = MODULES_REQUEST
        .response()
        .expect("Limine did not honor the modules request")
        .modules();
    task::process::init_spawnable_modules(boot_modules);

    arch::x86_64::init();
    earlyprintln!("[boot] GDT/TSS/IDT initialized");

    // Starts every additional CPU core Limine reported (if MP_REQUEST
    // was honored) and waits for each to report itself ready. Must run
    // before anything spawns a process: process-switch paths resolve
    // "which core is this" via `arch::x86_64::percpu::core_index()`,
    // which needs this call's `percpu::assign_slot(0, ..)` for the BSP
    // itself to have already run.
    match MP_REQUEST.response() {
        Some(resp) => arch::x86_64::smp::bring_up_aps(resp),
        None => arch::x86_64::smp::bring_up_bsp_only(),
    }

    // Must run only after the call above assigns the BSP's own percpu
    // slot (`percpu::assign_slot(0, ..)`): programming `LSTAR` needs to
    // know which of `syscall`'s per-core entry-stub copies belongs to
    // this core, via `percpu::core_index()`. Every AP programs its own
    // copy of these same MSRs as part of its own bring-up instead (see
    // `arch::x86_64::smp::ap_entry_on_own_stack`), for the same reason.
    arch::x86_64::syscall::init();
    earlyprintln!("[boot] SYSCALL/SYSRET initialized");

    // Eagerly, here, before anything spawns a process (and, critically,
    // before any test scenario ever snapshots
    // `memory::phys::free_frame_count()` as a baseline) -- see
    // `task::scheduler::init_idle_address_space`'s doc comment for why
    // building this lazily instead (the first time any core actually
    // went idle) looked exactly like a physical-memory leak. Must run
    // after the bring-up above so this address space's one-time PML4
    // snapshot already includes every core's idle stack.
    task::scheduler::init_idle_address_space();

    // Milestone 6: every additional core's idle loop (under this feature
    // only, see `arch::x86_64::smp::ap_entry_on_own_stack`) free-spins
    // incrementing its own counter instead of immediately hlt-parking.
    // After a fixed delay, logging every core's count that has genuinely
    // advanced -- and by a comparable order of magnitude across cores --
    // is real evidence they're executing concurrently, not secretly
    // serialized, without needing any periodic timer interrupt at all.
    // Never enabled for a normal build. See `xtask test-smp-boot`.
    #[cfg(feature = "smp-boot-test")]
    {
        use core::sync::atomic::Ordering;

        let start = arch::x86_64::interrupts::ticks();
        while arch::x86_64::interrupts::ticks() < start + 50 {
            x86_64::instructions::hlt();
        }

        for core_index in 0..arch::x86_64::percpu::MAX_CORES {
            let slot = arch::x86_64::percpu::slot(core_index);
            if slot.ready.load(Ordering::Acquire) {
                earlyprintln!(
                    "[smp-test] core {} spin_count={}",
                    core_index,
                    slot.spin_count.load(Ordering::Relaxed)
                );
            }
        }
        earlyprintln!("[smp-test] boot check complete");
    }

    // Milestone 6, adversarially: after bring-up, sends a targeted test
    // IPI to one specific booted core (not a broadcast) and confirms
    // only that core's IPI counter advanced, then sends the same IPI to
    // a LAPIC ID with no corresponding booted core and confirms the
    // kernel doesn't hang or fault. Never enabled for a normal build.
    // See `xtask test-smp-ipi`.
    #[cfg(feature = "smp-ipi-test")]
    {
        use core::sync::atomic::Ordering;

        const TARGET_INDEX: usize = 1;
        // Comfortably outside any LAPIC ID QEMU assigns for this
        // milestone's small `-smp` counts -- a real send to a target
        // with no corresponding booted core.
        const BAD_LAPIC_ID: u32 = 0xFE;

        let target_ready = arch::x86_64::percpu::slot(TARGET_INDEX)
            .ready
            .load(Ordering::Acquire);

        let result = if target_ready {
            let target_lapic_id = arch::x86_64::percpu::slot(TARGET_INDEX).lapic_id();
            arch::x86_64::lapic::send_ipi(target_lapic_id, arch::x86_64::lapic::TEST_IPI_VECTOR);

            let start = arch::x86_64::interrupts::ticks();
            while arch::x86_64::interrupts::ticks() < start + 10 {
                x86_64::instructions::hlt();
            }

            let mut ok = arch::x86_64::percpu::slot(TARGET_INDEX)
                .ipi_count
                .load(Ordering::Relaxed)
                == 1;
            for other in 0..arch::x86_64::percpu::MAX_CORES {
                if other != TARGET_INDEX
                    && arch::x86_64::percpu::slot(other).ipi_count.load(Ordering::Relaxed) != 0
                {
                    ok = false;
                }
            }

            arch::x86_64::lapic::send_ipi(BAD_LAPIC_ID, arch::x86_64::lapic::TEST_IPI_VECTOR);
            let start2 = arch::x86_64::interrupts::ticks();
            while arch::x86_64::interrupts::ticks() < start2 + 10 {
                x86_64::instructions::hlt();
            }

            ok
        } else {
            false
        };

        earlyprintln!("[smp-test] {}", if result { "IPI_OK" } else { "IPI_FAIL" });
    }

    // Milestone 11, Phase 4: PCI enumeration + virtio-blk driver bring-up
    // is unconditional starting here -- a real capability-gated syscall
    // (`SYS_BLOCK_READ`) needs the driver ready, or confirmed absent, on
    // every boot, disk attached or not, the same way `driver::uart::init`
    // below always runs regardless of which test feature (if any) is
    // active. `Err` just means no disk is attached this boot (every
    // scenario except the ones that explicitly attach one) -- never
    // itself a failure. See `driver::virtio_blk::init`'s own doc comment
    // for the PCI_ENUM_OK/FAIL log lines this produces (moved here from
    // this milestone's earlier Phase 2/3 test-only boot block, which
    // called `find_device` a second, redundant time just to print them).
    let _ = driver::virtio_blk::init();

    // Milestone 11, Phase 3: kernel-internal smoke test (no syscall yet
    // -- that's Phase 4, see `block-syscall-test` below) proving the
    // virtio-blk driver itself works end to end: read back a sector
    // `xtask` seeded with known content and confirm it matches exactly.
    // Sector 2, matching the fixed convention
    // `xtask::create_test_disk_image` uses. Never enabled for a normal
    // build.
    #[cfg(feature = "block-driver-test")]
    {
        const KNOWN_TEST_LBA: u64 = 2;
        use driver::block::BlockDevice;
        let result = driver::virtio_blk::with_device(|device| {
            let mut buf = [0u8; 512];
            device.read_sectors(KNOWN_TEST_LBA, &mut buf).map(|()| buf)
        });
        match result {
            Some(Ok(buf)) => {
                let matches = buf.iter().enumerate().all(|(i, &b)| b == (i % 256) as u8);
                earlyprintln!(
                    "[blk-test] {}",
                    if matches {
                        "BLOCK_READ_OK"
                    } else {
                        "BLOCK_READ_FAIL -- content mismatch"
                    }
                );
            }
            Some(Err(e)) => {
                earlyprintln!("[blk-test] BLOCK_READ_FAIL -- read_sectors error {:?}", e);
            }
            None => {
                earlyprintln!("[blk-test] BLOCK_READ_FAIL -- with_device found no driver");
            }
        }
    }

    // Milestone 11, Phase 4: spawns one dummy ring-3 process, seeds its
    // own (otherwise empty) capability table directly with `BLOCK_CAP`
    // (`Rights::READ` on a `KernelObjectRef::BlockDevice`) and
    // `CONSOLE_CAP` -- bypassing `init`/`SYS_GRANT` entirely, the same
    // "throwaway dummy process, capabilities seeded directly" pattern
    // every other kernel-feature-only test here uses (e.g.
    // `forced-preempt-test`'s own `ordinary_process`) -- then lets it
    // issue a real `SYS_BLOCK_READ` via raw inline asm and report
    // whether the content it read back matches. Proves the same content
    // this milestone's Phase 3 check already confirmed kernel-side is
    // *also* reachable through the real syscall/capability surface, from
    // genuine ring-3 code. Never enabled for a normal build.
    #[cfg(feature = "block-syscall-test")]
    {
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        let pid = task::scheduler::allocate_pid();
        let mut process = task::process::Process::new_dummy(
            pid,
            milestone11_tests::block_read_syscall_process,
            None,
        )
        .expect("failed to create the block-syscall-test process");
        process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        process.cap_table.insert(
            tarnos_abi::BLOCK_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::BlockDevice,
                rights: ipc::Rights::READ,
            },
        );
        task::scheduler::spawn(process).expect("spawn failed");
    }

    // Deliberately faults instead of continuing boot — see
    // `cargo run -p xtask -- test-fault`, which builds with this feature
    // specifically to confirm the page-fault/double-fault handling
    // produces a clean panic + halt rather than a triple fault. Never
    // enabled for a normal build.
    #[cfg(feature = "fault-injection-test")]
    unsafe {
        // Canonical (so this actually reaches the page-fault handler
        // rather than a general-protection fault on a malformed
        // address) but nowhere Limine's memory map would ever mark
        // usable, so it is guaranteed unmapped.
        let bad_ptr = 0x0000_1000_0000_0000u64 as *const u8;
        core::ptr::read_volatile(bad_ptr);
    }

    // Self-test: a breakpoint exception is non-fatal and the handler
    // returns, so reaching the next line proves the IDT is wired up
    // correctly rather than merely compiled.
    unsafe {
        asm!("int3", options(nomem, nostack));
    }
    earlyprintln!("[boot] breakpoint self-test passed");

    // Smoke test: if the heap allocator is wired up correctly, `Box` and
    // `Vec` (backed by real physical frames mapped in the previous step)
    // work exactly like they would with `std`.
    {
        use alloc::boxed::Box;
        use alloc::vec::Vec;

        let boxed = Box::new(0x7e57u64);
        assert_eq!(*boxed, 0x7e57);

        let mut v = Vec::new();
        for i in 0..1000u64 {
            v.push(i);
        }
        assert_eq!(v.len(), 1000);
        assert_eq!(v.iter().sum::<u64>(), 1000 * 999 / 2);

        earlyprintln!("[boot] heap smoke test passed (Box + 1000-element Vec)");
    }

    driver::uart::init();
    driver::uart::write_bytes(b"[uart] real 16550 driver online, this line went through it\r\n");
    earlyprintln!("[boot] UART driver initialized, IRQ4 unmasked (type to test echo)");

    task::executor::spawn(task::executor::Task::new(driver::uart::echo_task()));
    earlyprintln!("[boot] async executor started (UART RX echo task spawned)");

    // IPC smoke test: capability table rights-checking, plus the
    // rendezvous endpoint exercised in both wait orderings — the same
    // scenario the real init -> console_server handoff (below, in the
    // real boot sequence) relies on, and its mirror image, both covered
    // here with two kernel tasks standing in for the eventual process.
    {
        use alloc::sync::Arc;
        use ipc::{CapTable, CapabilitySlot, Endpoint, KernelObjectRef, Rights};
        use tarnos_abi::{CapIndex, Message, CONSOLE_CAP};

        let probe_endpoint = Arc::new(Endpoint::new());
        let mut table = CapTable::new();
        table.insert(
            CONSOLE_CAP,
            CapabilitySlot {
                object: KernelObjectRef::Endpoint(probe_endpoint),
                rights: Rights::SEND,
            },
        );
        assert!(table.lookup(CONSOLE_CAP, Rights::SEND).is_ok());
        assert!(table.lookup(CONSOLE_CAP, Rights::RECV).is_err());
        assert!(table.get(CapIndex(99)).is_err());
        earlyprintln!("[boot] capability table smoke test passed");

        // Receiver spawned (and so polled) before the sender.
        let receiver_first = Arc::new(Endpoint::new());
        let rf_recv = receiver_first.clone();
        task::executor::spawn(task::executor::Task::new(async move {
            let msg = rf_recv.recv().await;
            assert_eq!(msg.tag, 111);
            earlyprintln!("[boot] IPC smoke test (receiver-first) passed");
        }));
        task::executor::spawn(task::executor::Task::new(async move {
            receiver_first.send(Message::new(111, [0; 4])).await;
        }));

        // Sender spawned (and so polled) before the receiver.
        let sender_first = Arc::new(Endpoint::new());
        let sf_send = sender_first.clone();
        task::executor::spawn(task::executor::Task::new(async move {
            sf_send.send(Message::new(222, [0; 4])).await;
        }));
        task::executor::spawn(task::executor::Task::new(async move {
            let msg = sender_first.recv().await;
            assert_eq!(msg.tag, 222);
            earlyprintln!("[boot] IPC smoke test (sender-first) passed");
        }));
    }

    // Drains the executor once so the IPC smoke test tasks above (which
    // complete immediately, since a receiver is always either already
    // waiting or arrives in the same drain) actually run and print
    // before boot moves on to spawning init below.
    task::executor::run_ready_tasks();

    earlyprintln!("TarnOS kernel skeleton alive, idling.");

    // Milestone 2 integration test: two dummy ring-3 processes, one
    // that immediately faults and one that immediately exits cleanly —
    // confirms the fault kills only the offending process (see
    // `xtask test-fault-isolation`). Never enabled for a normal build.
    #[cfg(feature = "fault-isolation-test")]
    {
        let bad_pid = task::scheduler::allocate_pid();
        let bad_process =
            task::process::Process::new_dummy(bad_pid, milestone2_tests::faulting_process, None)
                .expect("failed to create the faulting dummy process");
        task::scheduler::spawn(bad_process).expect("spawn failed");

        let good_pid = task::scheduler::allocate_pid();
        let good_process =
            task::process::Process::new_dummy(good_pid, milestone2_tests::survivor_process, None)
                .expect("failed to create the survivor dummy process");
        task::scheduler::spawn(good_process).expect("spawn failed");

        earlyprintln!("[boot] fault-isolation-test: spawned faulting + survivor processes");
        task::scheduler::start();
    }

    // Milestone 2 integration test: two dummy ring-3 processes both
    // send on the same endpoint before any receiver is ever polled —
    // reproduces the historical double-send-panics bug, now expected to
    // queue both instead (see `xtask test-double-send`). Never enabled
    // for a normal build.
    #[cfg(feature = "double-send-test")]
    {
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));
        // Deliberately not polled yet -- both sends below must queue
        // (and not panic) before anything drains the console server.

        let pid_a = task::scheduler::allocate_pid();
        let mut process_a =
            task::process::Process::new_dummy(pid_a, milestone2_tests::sender_process_a, None)
                .expect("failed to create sender process A");
        process_a.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint.clone()),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(process_a).expect("spawn failed");

        let pid_b = task::scheduler::allocate_pid();
        let mut process_b =
            task::process::Process::new_dummy(pid_b, milestone2_tests::sender_process_b, None)
                .expect("failed to create sender process B");
        process_b.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(process_b).expect("spawn failed");

        earlyprintln!("[boot] double-send-test: spawned two senders, no receiver polled yet");
        task::scheduler::start();
    }

    // Milestone 3 integration test: an idle bystander process plus a
    // test process that adversarially probes SYS_GRANT/SYS_PROCESS_START
    // — a grant against a real process that isn't its child, a grant
    // requesting rights it doesn't hold, then a legitimate spawn+grant+
    // start to prove the mechanism still works (see
    // `xtask test-spawn-boundary`). Never enabled for a normal build.
    #[cfg(feature = "spawn-boundary-test")]
    {
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        // Allocated first, so it gets Pid(0) — the fixed value
        // `milestone3_tests::boundary_test_process`'s raw asm hardcodes
        // as "a real process that is not my child."
        let bystander_pid = task::scheduler::allocate_pid();
        let bystander_process = task::process::Process::new_dummy(
            bystander_pid,
            milestone3_tests::boundary_bystander_process,
            None,
        )
        .expect("failed to create the bystander dummy process");
        task::scheduler::spawn(bystander_process).expect("spawn failed");

        let test_pid = task::scheduler::allocate_pid();
        let mut test_process = task::process::Process::new_dummy(
            test_pid,
            milestone3_tests::boundary_test_process,
            None,
        )
        .expect("failed to create the boundary-test dummy process");
        // Only SEND, never RECV -- the process's own second check relies
        // on not holding RECV to prove a grant can't request more than
        // the granter itself has.
        test_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(test_process).expect("spawn failed");

        earlyprintln!("[boot] spawn-boundary-test: spawned bystander + boundary-test processes");
        task::scheduler::start();
    }

    // Milestone 4: repeatedly spawns a Suspended echo-child and
    // immediately kills it, well beyond MAX_PROCESSES times in a row —
    // confirms process teardown actually frees the process-table slot
    // and physical memory it used, instead of leaking either (see
    // `xtask test-process-lifecycle`). Never enabled for a normal build.
    #[cfg(feature = "process-lifecycle-test")]
    {
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        earlyprintln!(
            "[memtest] free_frames={}",
            memory::phys::free_frame_count()
        );

        let test_pid = task::scheduler::allocate_pid();
        let mut test_process = task::process::Process::new_dummy(
            test_pid,
            milestone4_tests::lifecycle_test_process,
            None,
        )
        .expect("failed to create the process-lifecycle-test dummy process");
        test_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(test_process).expect("spawn failed");

        earlyprintln!("[boot] process-lifecycle-test: spawned lifecycle-test process");
        task::scheduler::start();
    }

    // Milestone 4: spawns exit-code-child, releases it, and immediately
    // SYS_WAITs on it before it has ever run — forcing the wait to
    // genuinely block and later resume with the correct exit status
    // (see `xtask test-wait-exit-code`). Never enabled for a normal
    // build.
    #[cfg(feature = "wait-exit-code-test")]
    {
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        let test_pid = task::scheduler::allocate_pid();
        let mut test_process =
            task::process::Process::new_dummy(test_pid, milestone4_tests::wait_test_process, None)
                .expect("failed to create the wait-exit-code-test dummy process");
        test_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(test_process).expect("spawn failed");

        earlyprintln!("[boot] wait-exit-code-test: spawned wait-test process");
        task::scheduler::start();
    }

    // Milestone 4: an idle bystander process plus a test process that
    // adversarially probes SYS_KILL — a kill against a real process
    // that isn't its child, then legitimate kills against both a
    // Suspended and a genuinely Blocked real child (see
    // `xtask test-kill-boundary`). Never enabled for a normal build.
    #[cfg(feature = "kill-boundary-test")]
    {
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        // Allocated first, so it gets the fixed Pid (index 0, generation
        // 1) `milestone4_tests::kill_boundary_test_process`'s raw asm
        // hardcodes as "a real process that is not my child."
        let bystander_pid = task::scheduler::allocate_pid();
        let bystander_process = task::process::Process::new_dummy(
            bystander_pid,
            milestone4_tests::kill_test_bystander_process,
            None,
        )
        .expect("failed to create the bystander dummy process");
        task::scheduler::spawn(bystander_process).expect("spawn failed");

        let test_pid = task::scheduler::allocate_pid();
        let mut test_process = task::process::Process::new_dummy(
            test_pid,
            milestone4_tests::kill_boundary_test_process,
            None,
        )
        .expect("failed to create the kill-boundary-test dummy process");
        test_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        // A private SEND|RECV endpoint this process can grant into a
        // child it spawns, so that child can genuinely block on recv
        // (see check 3's doc comment on kill_boundary_test_process).
        test_process.cap_table.insert(
            tarnos_abi::CapIndex(1),
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(alloc::sync::Arc::new(ipc::Endpoint::new())),
                rights: ipc::Rights::SEND | ipc::Rights::RECV,
            },
        );
        task::scheduler::spawn(test_process).expect("spawn failed");

        earlyprintln!("[boot] kill-boundary-test: spawned bystander + kill-test processes");
        task::scheduler::start();
    }

    // Milestone 5: spawns heap-child (a real ELF process that proves
    // sys_sbrk-backed alloc works by building a multi-page Vec<u64>),
    // releases it, and SYS_WAITs for it to report success via its exit
    // code (see `xtask test-heap-growth`). Never enabled for a normal
    // build.
    #[cfg(feature = "heap-growth-test")]
    {
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        let test_pid = task::scheduler::allocate_pid();
        let mut test_process = task::process::Process::new_dummy(
            test_pid,
            milestone5_tests::heap_growth_test_process,
            None,
        )
        .expect("failed to create the heap-growth-test dummy process");
        test_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(test_process).expect("spawn failed");

        earlyprintln!("[boot] heap-growth-test: spawned heap-growth-test process");
        task::scheduler::start();
    }

    // Milestone 5: adversarially probes SYS_SBRK directly — an absurd
    // increment over the fixed heap ceiling, a valid grow, a rejected
    // negative increment, and a side-effect-free zero-increment query
    // (see `xtask test-sbrk-boundary`). Never enabled for a normal build.
    #[cfg(feature = "sbrk-boundary-test")]
    {
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        let test_pid = task::scheduler::allocate_pid();
        let mut test_process = task::process::Process::new_dummy(
            test_pid,
            milestone5_tests::sbrk_boundary_test_process,
            None,
        )
        .expect("failed to create the sbrk-boundary-test dummy process");
        test_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(test_process).expect("spawn failed");

        earlyprintln!("[boot] sbrk-boundary-test: spawned sbrk-boundary-test process");
        task::scheduler::start();
    }

    // Milestone 7: spawns two dummy processes that each free-spin on
    // SYS_YIELD, then polls every core's current-process atomic looking
    // for direct evidence both are resident on two *different* cores at
    // the same instant -- real proof of concurrent, cross-core process
    // execution, not just "eventually scheduled somewhere" (see
    // `xtask test-smp-sched-concurrency`). Never enabled for a normal
    // build.
    #[cfg(feature = "smp-sched-concurrency-test")]
    {
        let pid_a = task::scheduler::allocate_pid();
        let process_a =
            task::process::Process::new_dummy(pid_a, milestone7_tests::concurrency_process_a, None)
                .expect("failed to create concurrency test process A");
        task::scheduler::spawn(process_a).expect("spawn failed");

        let pid_b = task::scheduler::allocate_pid();
        let process_b =
            task::process::Process::new_dummy(pid_b, milestone7_tests::concurrency_process_b, None)
                .expect("failed to create concurrency test process B");
        task::scheduler::spawn(process_b).expect("spawn failed");

        earlyprintln!("[boot] smp-sched-concurrency-test: spawned processes A and B");

        let mut saw_concurrent = false;
        for _ in 0..200 {
            let start = arch::x86_64::interrupts::ticks();
            while arch::x86_64::interrupts::ticks() < start + 2 {
                x86_64::instructions::hlt();
            }

            let mut core_of_a = None;
            let mut core_of_b = None;
            for core in 0..arch::x86_64::percpu::MAX_CORES {
                let current = arch::x86_64::percpu::slot(core)
                    .current
                    .load(core::sync::atomic::Ordering::Acquire);
                if current == pid_a.0 {
                    core_of_a = Some(core);
                }
                if current == pid_b.0 {
                    core_of_b = Some(core);
                }
            }
            if let (Some(core_a), Some(core_b)) = (core_of_a, core_of_b) {
                if core_a != core_b {
                    earlyprintln!(
                        "[smp-test] pid_a on core {core_a}, pid_b on core {core_b} -- concurrent"
                    );
                    saw_concurrent = true;
                    break;
                }
            }
        }
        earlyprintln!(
            "[smp-test] {}",
            if saw_concurrent { "CONCURRENCY_OK" } else { "CONCURRENCY_FAIL" }
        );

        task::scheduler::start();
    }

    // Milestone 7: spawns a dummy process that free-spins forever as the
    // child of a second dummy process that yields a few times (letting
    // the first one actually start running, most likely on a different
    // core) and then SYS_KILLs it -- the direct adversarial test for
    // `task::scheduler::terminate_process`'s cross-core eviction
    // protocol. Immediately afterward the killer spawns, starts, and
    // waits on one more ordinary child to confirm the machine is still
    // fully healthy right after the eviction (see
    // `xtask test-smp-kill-cross-core`). Never enabled for a normal
    // build.
    #[cfg(feature = "kill-cross-core-test")]
    {
        // Allocated first, so it gets the fixed Pid (index 0, generation
        // 1) `milestone7_tests::kill_cc_killer_process` hardcodes as its
        // target -- see that function's doc comment.
        //
        // Spawned immediately, with a placeholder `parent: None`, rather
        // than allocating `killer_pid` first and passing it in here
        // directly: `allocate_pid` only reserves a `Pid` value (bumping
        // that slot's generation) -- it does *not* mark the table slot
        // non-`Empty`, which only `spawn`/`spawn_suspended` do. Calling
        // it a second time (for `killer_pid`) before this process was
        // ever actually spawned would find the *same* slot still
        // reporting `Empty` and hand out that same index again, just
        // one generation higher -- exactly the hazard `allocate_pid`'s
        // own doc comment warns callers must avoid. A real, reproduced
        // bug: both pids ended up naming table index 0, so target's own
        // `spawn` and killer's `spawn` right after it landed in the
        // *same* slot, and the ready queue ended up with two different
        // `Pid`s (different generations) both resolving to one shared
        // `Process` -- silent double-scheduling that surfaced as
        // wild jumps to near-null addresses under real cross-core
        // timing (`xtask test-smp-kill-cross-core`). Spawning target
        // first (so its slot is genuinely `Occupied` before `killer_pid`
        // is ever allocated) closes the gap; its `parent` is fixed up
        // below once `killer_pid` is actually known.
        let target_pid = task::scheduler::allocate_pid();
        let target_process = task::process::Process::new_dummy(
            target_pid,
            milestone7_tests::kill_cc_target_process,
            None,
        )
        .expect("failed to create the kill-cross-core-test target process");
        task::scheduler::spawn(target_process).expect("spawn failed");

        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        let killer_pid = task::scheduler::allocate_pid();
        task::scheduler::with_process(target_pid, |p| p.parent = Some(killer_pid))
            .expect("target process vanished before its parent could be set");

        let mut killer_process = task::process::Process::new_dummy(
            killer_pid,
            milestone7_tests::kill_cc_killer_process,
            None,
        )
        .expect("failed to create the kill-cross-core-test killer process");
        killer_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(killer_process).expect("spawn failed");

        earlyprintln!("[boot] kill-cross-core-test: spawned target + killer processes");
        task::scheduler::start();
    }

    // Milestone 8: spawns a receiver process that immediately `SYS_RECV`s
    // on a fresh endpoint (blocking, since nothing has been sent yet)
    // and a sender process that immediately `SYS_SEND`s the same
    // message -- spawned in that order specifically so the receiver is
    // very likely already blocked on a *different*, previously-idle
    // core by the time the sender delivers, forcing the exact
    // `Process::pending_wake` race window (see `docs/adr/0011`). Never
    // enabled for a normal build.
    #[cfg(feature = "smp-send-cross-core-test")]
    {
        let test_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        let receiver_pid = task::scheduler::allocate_pid();
        let mut receiver_process = task::process::Process::new_dummy(
            receiver_pid,
            milestone8_tests::send_cc_receiver_process,
            None,
        )
        .expect("failed to create the send-cross-core-test receiver process");
        receiver_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        receiver_process.cap_table.insert(
            tarnos_abi::CapIndex(1),
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(test_endpoint.clone()),
                rights: ipc::Rights::RECV,
            },
        );
        task::scheduler::spawn(receiver_process).expect("spawn failed");

        let sender_pid = task::scheduler::allocate_pid();
        let mut sender_process = task::process::Process::new_dummy(
            sender_pid,
            milestone8_tests::send_cc_sender_process,
            None,
        )
        .expect("failed to create the send-cross-core-test sender process");
        sender_process.cap_table.insert(
            tarnos_abi::CapIndex(1),
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(test_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(sender_process).expect("spawn failed");

        earlyprintln!("[boot] smp-send-cross-core-test: spawned receiver + sender processes");
        task::scheduler::start();
    }

    // Milestone 8: spawns a dummy process that busy-loops forever
    // *without ever making a single syscall* -- the only thing that can
    // ever preempt it is the new per-core LAPIC timer. Polls every
    // core's current-process atomic to find out empirically which core
    // it landed on, then keeps watching that exact core for direct
    // evidence it gets preempted there -- the same "confirm, don't
    // assume" methodology `smp-sched-concurrency-test` above already
    // uses. Afterward spawns an ordinary process (confirming it still
    // gets to run and exit cleanly) and a killer process that `SYS_KILL`s
    // the busy one -- the direct adversarial test that cross-core
    // eviction still works when the target was forcibly, not
    // cooperatively, scheduled the whole time. Never enabled for a
    // normal build.
    #[cfg(feature = "forced-preempt-test")]
    {
        // Spawned first, so it's very likely picked up by an idle AP
        // before this boot code (still running on the BSP) ever reaches
        // `task::scheduler::start()` itself -- see
        // `milestone7_tests::kill_cc_killer_process`'s doc comment for
        // the same reasoning.
        let busy_pid = task::scheduler::allocate_pid();
        let busy_process = task::process::Process::new_dummy(
            busy_pid,
            milestone8_tests::forced_preempt_busy_process,
            None,
        )
        .expect("failed to create the forced-preempt-test busy process");
        task::scheduler::spawn(busy_process).expect("spawn failed");

        let mut busy_core = None;
        for _ in 0..200 {
            let start = arch::x86_64::interrupts::ticks();
            while arch::x86_64::interrupts::ticks() < start + 1 {
                x86_64::instructions::hlt();
            }
            for core in 0..arch::x86_64::percpu::MAX_CORES {
                if arch::x86_64::percpu::slot(core)
                    .current
                    .load(core::sync::atomic::Ordering::Acquire)
                    == busy_pid.0
                {
                    busy_core = Some(core);
                    break;
                }
            }
            if busy_core.is_some() {
                break;
            }
        }
        let busy_core = busy_core.expect("forced-preempt-test busy process never started running");

        // Not "does `current` ever change" -- with nothing else ever
        // ready on this core, a preempted busy process is immediately
        // redispatched right back to itself, so `current` never actually
        // changes value even though real preemption keeps happening.
        // `preempt_count` is bumped unconditionally on every preemption
        // regardless of what gets dispatched next, so it's the one
        // signal that can't hide that case.
        let mut saw_preemption = false;
        for _ in 0..400 {
            let start = arch::x86_64::interrupts::ticks();
            while arch::x86_64::interrupts::ticks() < start + 1 {
                x86_64::instructions::hlt();
            }
            let preempt_count = arch::x86_64::percpu::slot(busy_core)
                .preempt_count
                .load(core::sync::atomic::Ordering::Acquire);
            if preempt_count > 0 {
                saw_preemption = true;
                break;
            }
        }
        earlyprintln!(
            "[forced-preempt-test] busy process ran on core {busy_core}, {}",
            if saw_preemption { "PREEMPTION_OK" } else { "PREEMPTION_FAIL" }
        );

        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        let ordinary_pid = task::scheduler::allocate_pid();
        let mut ordinary_process = task::process::Process::new_dummy(
            ordinary_pid,
            milestone8_tests::forced_preempt_ordinary_process,
            None,
        )
        .expect("failed to create the forced-preempt-test ordinary process");
        ordinary_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint.clone()),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(ordinary_process).expect("spawn failed");

        // Allocated (and its parent fixed up) only after `busy_process`
        // was actually spawned -- see `kill_cc_killer_process`'s doc
        // comment on why `allocate_pid` must never be called a second
        // time before the first allocation's own `spawn` lands.
        let killer_pid = task::scheduler::allocate_pid();
        task::scheduler::with_process(busy_pid, |p| p.parent = Some(killer_pid))
            .expect("forced-preempt-test busy process vanished before its parent could be set");

        let mut killer_process = task::process::Process::new_dummy(
            killer_pid,
            milestone8_tests::forced_preempt_killer_process,
            None,
        )
        .expect("failed to create the forced-preempt-test killer process");
        killer_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(killer_process).expect("spawn failed");

        earlyprintln!("[boot] forced-preempt-test: spawned busy + ordinary + killer processes");
        task::scheduler::start();
    }

    // Milestone 9's combined "kitchen sink" scenario: several small
    // orchestrator processes, each driving a *different*,
    // already-proven-solid workload concurrently rather than in
    // isolation -- an IPC round trip via echo-child, heap growth via
    // heap-child, a bounded spawn+wait lifecycle loop via
    // exit-code-child, a kill-mid-flight orchestrator against a
    // never-yielding target, and background SYS_YIELD pressure
    // processes keeping every core genuinely busy throughout. Peak
    // concurrent process-table usage stays comfortably under
    // `task::scheduler::MAX_PROCESSES` (16). Never enabled for a normal
    // build.
    #[cfg(feature = "kitchen-sink-test")]
    {
        // Allocated and spawned first, with a placeholder `parent: None`
        // -- same reasoning as `kill-cross-core-test`'s own boot code --
        // so `kitchen_sink_tests::ks_kill_orchestrator`'s hardcoded
        // `TARGET_PID` (table index 0, generation 1) is correct.
        let kill_target_pid = task::scheduler::allocate_pid();
        let kill_target_process = task::process::Process::new_dummy(
            kill_target_pid,
            kitchen_sink_tests::ks_kill_target_process,
            None,
        )
        .expect("failed to create the kitchen-sink-test kill target process");
        task::scheduler::spawn(kill_target_process).expect("spawn failed");

        let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
        task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
            console_endpoint.clone(),
        )));

        let ipc_pid = task::scheduler::allocate_pid();
        let mut ipc_process = task::process::Process::new_dummy(
            ipc_pid,
            kitchen_sink_tests::ks_ipc_orchestrator,
            None,
        )
        .expect("failed to create the kitchen-sink-test IPC orchestrator");
        ipc_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint.clone()),
                rights: ipc::Rights::SEND,
            },
        );
        // The orchestrator's own link endpoint, reserved for talking to
        // whatever child it spawns -- mirrors `tarnos_abi::CHILD_LINK_CAP`'s
        // role for `init`, just seeded here instead of by the boot-module
        // registry.
        ipc_process.cap_table.insert(
            tarnos_abi::CapIndex(1),
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(alloc::sync::Arc::new(ipc::Endpoint::new())),
                rights: ipc::Rights::SEND | ipc::Rights::RECV,
            },
        );
        task::scheduler::spawn(ipc_process).expect("spawn failed");

        let heap_pid = task::scheduler::allocate_pid();
        let mut heap_process = task::process::Process::new_dummy(
            heap_pid,
            kitchen_sink_tests::ks_heap_orchestrator,
            None,
        )
        .expect("failed to create the kitchen-sink-test heap orchestrator");
        heap_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint.clone()),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(heap_process).expect("spawn failed");

        let lifecycle_pid = task::scheduler::allocate_pid();
        let mut lifecycle_process = task::process::Process::new_dummy(
            lifecycle_pid,
            kitchen_sink_tests::ks_lifecycle_orchestrator,
            None,
        )
        .expect("failed to create the kitchen-sink-test lifecycle orchestrator");
        lifecycle_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint.clone()),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(lifecycle_process).expect("spawn failed");

        let kill_orchestrator_pid = task::scheduler::allocate_pid();
        task::scheduler::with_process(kill_target_pid, |p| p.parent = Some(kill_orchestrator_pid))
            .expect("kitchen-sink-test kill target vanished before its parent could be set");
        let mut kill_orchestrator_process = task::process::Process::new_dummy(
            kill_orchestrator_pid,
            kitchen_sink_tests::ks_kill_orchestrator,
            None,
        )
        .expect("failed to create the kitchen-sink-test kill orchestrator");
        kill_orchestrator_process.cap_table.insert(
            tarnos_abi::CONSOLE_CAP,
            ipc::CapabilitySlot {
                object: ipc::KernelObjectRef::Endpoint(console_endpoint),
                rights: ipc::Rights::SEND,
            },
        );
        task::scheduler::spawn(kill_orchestrator_process).expect("spawn failed");

        // Background scheduling pressure: no capabilities needed at all,
        // since these never do IPC -- just SYS_YIELD load spread across
        // every core alongside the real workloads above. Two, not one
        // per core (four): see `ks_pressure_process`'s own doc comment
        // on why this is deliberately light background contention, not
        // this scenario's own dominant workload.
        for _ in 0..2 {
            let pressure_pid = task::scheduler::allocate_pid();
            let pressure_process = task::process::Process::new_dummy(
                pressure_pid,
                kitchen_sink_tests::ks_pressure_process,
                None,
            )
            .expect("failed to create a kitchen-sink-test pressure process");
            task::scheduler::spawn(pressure_process).expect("spawn failed");
        }

        earlyprintln!("[boot] kitchen-sink-test: spawned all orchestrator + pressure processes");
        task::scheduler::start();
    }

    // The real boot sequence: spawn and poll the console server to its
    // first `recv().await` (so it's registered as a waiting receiver)
    // *before* init is created and scheduled — eliminating the
    // sender-before-receiver race by construction rather than by
    // handling both orderings at runtime.
    //
    // Milestone 2 integration test (`blocking-ipc-test`, see
    // `xtask test-blocking-ipc`): skips priming the console server here,
    // forcing init's first `sys_send` below to find nobody ready and
    // genuinely block — proving a process can actually suspend and
    // later resume, not just that this always-primed-receiver ordering
    // happens to work. Never enabled for a normal build.
    let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
    task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
        console_endpoint.clone(),
    )));
    #[cfg(not(feature = "blocking-ipc-test"))]
    {
        task::executor::run_ready_tasks();
        earlyprintln!("[boot] console server started, waiting for messages");
    }

    let init_module = boot_modules
        .iter()
        .find(|module| module.cmdline() == "init")
        .expect("no boot module with cmdline \"init\" (check limine.conf's module_string)");

    let init_pid = task::scheduler::allocate_pid();
    let mut init_process = task::process::Process::from_elf(init_pid, init_module.data(), None)
        .expect("failed to load init's ELF image");
    init_process.cap_table.insert(
        tarnos_abi::CONSOLE_CAP,
        ipc::CapabilitySlot {
            object: ipc::KernelObjectRef::Endpoint(console_endpoint),
            rights: ipc::Rights::SEND,
        },
    );
    // init otherwise only holds SEND (on CONSOLE_CAP) — without a RECV
    // right of its own to grant, it would have nothing to hand a
    // spawned child to talk back with.
    init_process.cap_table.insert(
        tarnos_abi::CHILD_LINK_CAP,
        ipc::CapabilitySlot {
            object: ipc::KernelObjectRef::Endpoint(alloc::sync::Arc::new(ipc::Endpoint::new())),
            rights: ipc::Rights::SEND | ipc::Rights::RECV,
        },
    );
    task::scheduler::spawn(init_process).expect("process table exhausted spawning the very first process");

    // scheduler::start() never returns: once a real process exists, the
    // machine is its (and the scheduler's) from here on, driven by the
    // timer's forced preemption — the kernel's own idle loop above never
    // runs again after this point. Kernel tasks (the UART echo task, the
    // console server) still keep making progress from here on too, not
    // via that idle loop but because `task::scheduler::on_timer_tick`
    // itself drains the executor's ready queue on every tick — see that
    // function's own doc comment. init calling `sys_exit` with nothing
    // else scheduled falls back to `scheduler::on_syscall_exit`'s halt,
    // which is this milestone's clean terminal state.
    earlyprintln!("[boot] spawning init...");
    task::scheduler::start();
}
