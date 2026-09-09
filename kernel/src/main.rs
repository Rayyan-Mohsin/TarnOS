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

    // The real boot sequence: spawn and poll the console server to its
    // first `recv().await` (so it's registered as a waiting receiver)
    // *before* init is created and scheduled — eliminating the
    // sender-before-receiver race by construction rather than by
    // handling both orderings at runtime.
    let console_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());
    task::executor::spawn(task::executor::Task::new(driver::uart::console_server(
        console_endpoint.clone(),
    )));
    task::executor::run_ready_tasks();
    earlyprintln!("[boot] console server started, waiting for messages");

    let init_module = MODULES_REQUEST
        .response()
        .expect("Limine did not honor the modules request")
        .modules()
        .iter()
        .find(|module| module.cmdline() == "init")
        .expect("no boot module with cmdline \"init\" (check limine.conf's module_string)");

    let init_pid = task::scheduler::allocate_pid();
    let mut init_process = task::process::Process::from_elf(init_pid, init_module.data())
        .expect("failed to load init's ELF image");
    init_process.cap_table.insert(
        tarnos_abi::CONSOLE_CAP,
        ipc::CapabilitySlot {
            object: ipc::KernelObjectRef::Endpoint(console_endpoint),
            rights: ipc::Rights::SEND,
        },
    );
    task::scheduler::spawn(init_process);

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
