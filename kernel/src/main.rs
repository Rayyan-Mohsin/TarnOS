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

    let mut executor = task::executor::Executor::new();
    executor.spawn(task::executor::Task::new(driver::uart::echo_task()));
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
        executor.spawn(task::executor::Task::new(async move {
            let msg = rf_recv.recv().await;
            assert_eq!(msg.tag, 111);
            earlyprintln!("[boot] IPC smoke test (receiver-first) passed");
        }));
        executor.spawn(task::executor::Task::new(async move {
            receiver_first.send(Message::new(111, [0; 4])).await;
        }));

        // Sender spawned (and so polled) before the receiver.
        let sender_first = Arc::new(Endpoint::new());
        let sf_send = sender_first.clone();
        executor.spawn(task::executor::Task::new(async move {
            sf_send.send(Message::new(222, [0; 4])).await;
        }));
        executor.spawn(task::executor::Task::new(async move {
            let msg = sender_first.recv().await;
            assert_eq!(msg.tag, 222);
            earlyprintln!("[boot] IPC smoke test (sender-first) passed");
        }));
    }

    // Drains the executor once so the IPC smoke test tasks above (which
    // complete immediately, since a receiver is always either already
    // waiting or arrives in the same drain) actually run and print
    // before boot moves on to the scheduler test below, which never
    // returns to this idle loop.
    executor.run_ready_tasks();

    earlyprintln!("TarnOS kernel skeleton alive, idling.");

    // SYSCALL smoke test: two ring-3 "dummy processes" exercising every
    // syscall this milestone defines. C sends a tagged message then
    // exits (SYS_SEND, SYS_EXIT); D polls for it non-blockingly,
    // yielding between attempts until it arrives (SYS_RECV, SYS_YIELD),
    // then parks with the received tag in rbx — observable the same way
    // the milestone-12 scheduler test observed its dummy processes'
    // registers, proving the message's payload actually made the round
    // trip through the syscall ABI and the rendezvous endpoint, not just
    // that the instructions didn't crash. Both processes share one
    // capability slot 0, pointing at the same endpoint with the rights
    // each side actually needs (never both — that's the point of
    // capabilities: C cannot receive on this endpoint, D cannot send).
    //
    // scheduler::start() never returns: once real processes exist, the
    // machine is theirs, driven by the timer's forced preemption between
    // them — the kernel's own idle loop above never runs again after
    // this point. That's a real, if simplified, limitation of this
    // milestone (kernel tasks and processes aren't yet time-sliced
    // against processes) rather than a bug; a later milestone task
    // integrates them.
    let shared_endpoint = alloc::sync::Arc::new(ipc::Endpoint::new());

    let pid_c = task::scheduler::allocate_pid();
    let mut process_c = task::process::Process::new_dummy(pid_c, dummy_process_c)
        .expect("failed to build dummy process C");
    process_c.cap_table.insert(
        tarnos_abi::CapIndex(0),
        ipc::CapabilitySlot {
            object: ipc::KernelObjectRef::Endpoint(shared_endpoint.clone()),
            rights: ipc::Rights::SEND,
        },
    );
    task::scheduler::spawn(process_c);

    let pid_d = task::scheduler::allocate_pid();
    let mut process_d = task::process::Process::new_dummy(pid_d, dummy_process_d)
        .expect("failed to build dummy process D");
    process_d.cap_table.insert(
        tarnos_abi::CapIndex(0),
        ipc::CapabilitySlot {
            object: ipc::KernelObjectRef::Endpoint(shared_endpoint),
            rights: ipc::Rights::RECV,
        },
    );
    task::scheduler::spawn(process_d);

    earlyprintln!("[boot] syscall smoke-test processes spawned, starting scheduler...");
    task::scheduler::start();
}

/// Sends one tagged message on capability slot 0, then exits.
/// Self-contained asm (not ordinary Rust) so it fits in a single page
/// with no working stack, matching `Process::new_dummy`'s requirements.
unsafe extern "C" fn dummy_process_c() -> ! {
    unsafe {
        asm!(
            "xor edi, edi",   // cap index 0
            "mov esi, 12345", // tag
            "xor edx, edx",
            "xor r10d, r10d",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "mov eax, 1", // SYS_SEND
            "syscall",
            "xor edi, edi", // exit code 0
            "mov eax, 3",   // SYS_EXIT
            "syscall",
            "2:",
            "jmp 2b",
            options(noreturn)
        );
    }
}

/// Polls capability slot 0 for a message, yielding between non-blocking
/// attempts, then parks with the received tag in `rbx` once one arrives.
unsafe extern "C" fn dummy_process_d() -> ! {
    unsafe {
        asm!(
            "2:",
            "xor edi, edi", // cap index 0
            "mov eax, 2",   // SYS_RECV
            "syscall",
            "test rax, rax",
            "jz 3f",
            "mov eax, 0", // SYS_YIELD
            "syscall",
            "jmp 2b",
            "3:",
            "mov rbx, rdi", // success: stash the received tag
            "4:",
            "jmp 4b",
            options(noreturn)
        );
    }
}
