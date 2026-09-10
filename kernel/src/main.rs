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
mod sync;
mod task;

use core::arch::asm;
use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, ModulesRequest,
    RsdpRequest, StackSizeRequest,
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
