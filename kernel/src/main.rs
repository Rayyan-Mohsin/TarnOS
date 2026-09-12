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

/// Captured now, unused, so a future migration to LAPIC/IOAPIC/SMP (which
/// needs ACPI tables) doesn't require a boot-protocol change.
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
    // rendezvous endpoint exercised in both wait orderings — the
    // scenario the real init -> console_server handoff (a later
    // milestone task) relies on, and its mirror image, both covered here
    // with two kernel tasks standing in for the eventual process.
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
    // runs again after this point. That's a real, if simplified,
    // limitation of this milestone (kernel tasks and processes aren't
    // yet time-sliced against each other) rather than a bug; a later
    // milestone task integrates them. init calling `sys_exit` with
    // nothing else scheduled falls back to `scheduler::on_syscall_exit`'s
    // halt, which is this milestone's clean terminal state.
    earlyprintln!("[boot] spawning init...");
    task::scheduler::start();
}
