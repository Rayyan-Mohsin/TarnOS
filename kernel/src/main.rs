//! TarnOS kernel entry point.
//!
//! This file wires up the Limine boot protocol requests and hands off to
//! the rest of the kernel. Early boot output goes through a bare, direct
//! port-I/O poke (see [`early_print`]) rather than the real UART driver
//! (`driver::uart`, added later) so boot can be proven working before any
//! other subsystem exists.
#![no_std]
#![no_main]

mod lang_items;

use core::arch::asm;
use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, ModulesRequest,
    RsdpRequest,
};
use limine::BaseRevision;

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

/// Writes a byte directly to the COM1 I/O port with no UART initialization.
///
/// QEMU's 16550 emulation accepts bytes on THR (0x3F8) without prior DLAB/
/// FIFO setup, so this is sufficient to prove boot+link+ISO+QEMU works
/// before the real driver (`driver::uart`, with proper initialization and
/// LSR busy-checking, needed on real hardware) exists.
fn early_print(s: &str) {
    for byte in s.bytes() {
        unsafe {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nomem, nostack, preserves_flags));
        }
    }
}

#[no_mangle]
extern "C" fn _start() -> ! {
    assert!(BASE_REVISION.is_supported(), "unsupported Limine base revision");

    early_print("TarnOS booting...\r\n");

    if let Some(resp) = MEMMAP_REQUEST.response() {
        early_print("[boot] memory map received\r\n");
        let _ = resp.entries().len();
    }
    if let Some(resp) = HHDM_REQUEST.response() {
        early_print("[boot] HHDM offset received\r\n");
        let _ = resp.offset;
    }
    if EXECUTABLE_ADDRESS_REQUEST.response().is_some() {
        early_print("[boot] kernel address received\r\n");
    }
    if let Some(resp) = MODULES_REQUEST.response() {
        early_print("[boot] boot modules received\r\n");
        let _ = resp.modules().len();
    }
    if RSDP_REQUEST.response().is_some() {
        early_print("[boot] RSDP received\r\n");
    }

    early_print("TarnOS kernel skeleton alive, halting.\r\n");

    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}
