//! Bringing up additional CPU cores via Limine's `MpRequest`.
//!
//! Limine's crate documents the `MpInfo::bootstrap()` handshake mechanics
//! (write an argument, then a goto pointer, and the parked core jumps to
//! it) but says nothing about what CPU/paging/GDT/stack state that core
//! is actually in when it arrives — so [`ap_entry_trampoline`] below
//! assumes as little as possible: it does no Rust-level calls and
//! touches no memory beyond reading its one argument out of the
//! `MpInfo` it was handed, until it has switched onto a stack this
//! kernel mapped itself. Only once safely there does
//! [`ap_entry_on_own_stack`] log what it actually inherited (the
//! empirical check `docs/adr/0009` calls for) before doing anything
//! else.
use core::sync::atomic::Ordering;

use limine::mp::{MpGotoFunction, MpInfo, MpRespData};
use x86_64::registers::control::Cr3;
use x86_64::registers::rflags::{self, RFlags};
use x86_64::structures::paging::{FrameAllocator, Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

use super::{gdt, idt, lapic, percpu};
use crate::earlyprintln;
use crate::memory::phys::GlobalFrameAllocator;
use crate::memory::virt;

const IDLE_STACK_PAGES: u64 = 4;
/// Fixed virtual base for per-core idle stacks — clear of the kernel heap
/// (`0xffff_9000_0000_0000`), the per-core double-fault stack region
/// (`0xffff_9400_0000_0000`, see `gdt`), and the per-process kernel stack
/// region (`0xffff_9800_0000_0000`, see `task::process`).
const IDLE_STACK_BASE: u64 = 0xffff_9600_0000_0000;
/// Guard page + stack, one slot per core index — same shape as every
/// other per-core/per-process fixed region in this kernel.
const IDLE_STACK_SLOT_STRIDE: u64 = 4096 * (1 + IDLE_STACK_PAGES);

fn idle_stack_slot_base(core_index: usize) -> u64 {
    IDLE_STACK_BASE + core_index as u64 * IDLE_STACK_SLOT_STRIDE
}

/// Maps `core_index`'s idle stack and returns its top, leaving a guard
/// page immediately below it — mirrors `gdt::double_fault_stack_top`
/// exactly. Called for every possible core index eagerly, by the BSP,
/// before any AP is started: an AP cannot safely map its own stack
/// before it has verified its own landing state, so this must already
/// exist by the time [`ap_entry_trampoline`] computes the same address
/// via the matching arithmetic baked into its `global_asm!` body below.
fn idle_stack_top(core_index: usize) -> VirtAddr {
    let mut allocator = GlobalFrameAllocator;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    let slot_base = idle_stack_slot_base(core_index);
    for i in 1..=IDLE_STACK_PAGES {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(slot_base + i * 4096));
        let frame = allocator
            .allocate_frame()
            .expect("out of memory mapping an idle stack");
        virt::map(page, frame, flags).expect("failed to map idle stack page");
    }
    VirtAddr::new(slot_base + (1 + IDLE_STACK_PAGES) * 4096)
}

// The raw AP landing point handed to `MpInfo::bootstrap()`. Limine calls
// this with a normal `extern "C"` convention (rdi = &MpInfo) — there is
// no hardware trap frame to save/restore the way the timer/fault entry
// stubs need, but the incoming stack's validity is exactly the
// undocumented gap this module's doc comment names, so this does the
// least possible before switching onto a stack this kernel mapped
// itself: read `extra_argument` (the core index) directly out of
// `MpInfo` by its known `#[repr(C)]` field offset (24 — see the type
// definition in the vendored `limine` 0.6.5 crate: `processor_id: u32`
// + `lapic_id: u32` + 8 bytes reserved + `goto_addr: AtomicPtr<()>` land
// `extra_argument` at byte 24), bounds-check it, compute that core's
// idle-stack top by the same arithmetic `idle_stack_top` above uses, and
// jump there before calling into any further Rust code.
core::arch::global_asm!(
    ".global ap_entry_trampoline",
    "ap_entry_trampoline:",
    "mov r10, [rdi + 24]",   // r10 = core index (extra_argument), kept safe across the multiply below
    "cmp r10, {max_cores}",
    "jae 2f",                // out of range -- park this core, never touch memory again
    "mov rax, r10",
    "mov rcx, {stride}",
    "mul rcx",               // rax = core_index * stride (rdx ignored -- core counts are tiny)
    "mov rsp, {stack_region_top}", // = idle_stack_top(0)
    "add rsp, rax",          // += core_index * stride == idle_stack_top(core_index)
    "mov rdi, r10",
    "call {next}",
    "2:",
    "cli",
    "3:",
    "hlt",
    "jmp 3b",
    max_cores = const percpu::MAX_CORES as u64,
    stride = const IDLE_STACK_SLOT_STRIDE,
    stack_region_top = const IDLE_STACK_BASE + (1 + IDLE_STACK_PAGES) * 4096,
    next = sym ap_entry_on_own_stack,
);

unsafe extern "C" {
    fn ap_entry_trampoline(info: &MpInfo) -> !;
}

/// Runs once, on its own stack, for exactly one AP. Logs what CPU state
/// it actually landed in (the empirical check this whole module exists
/// to make, rather than assume), then finishes this core's own bring-up:
/// GDT/TSS, IDT, LAPIC, mark itself ready, enable interrupts, and settle
/// into an idle loop.
#[unsafe(no_mangle)]
extern "C" fn ap_entry_on_own_stack(core_index: u64) -> ! {
    let core_index = core_index as usize;

    let (cr3_frame, _) = Cr3::read();
    let flags = rflags::read();
    earlyprintln!(
        "[smp] core {} landed: cr3={:#x} rflags.if={}",
        core_index,
        cr3_frame.start_address().as_u64(),
        flags.contains(RFlags::INTERRUPT_FLAG),
    );

    unsafe {
        // IDT first: if GDT/TSS bring-up below somehow faults, this
        // core at least has a valid IDT to catch it with a diagnostic
        // instead of silently triple-faulting (see `gdt::init_ap`'s doc
        // comment for exactly this failure mode, hit during this
        // milestone's own development).
        idt::load_ap();
        gdt::init_ap(core_index);
        lapic::init_this_core();
    }

    percpu::slot(core_index).ready.store(true, Ordering::Release);
    earlyprintln!("[smp] core {} ready", core_index);

    x86_64::instructions::interrupts::enable();

    #[cfg(feature = "smp-boot-test")]
    {
        // Free-spin, incrementing this core's own counter, as evidence
        // (see `xtask test-smp-boot`) that every core is genuinely
        // executing concurrently rather than being secretly serialized.
        // Still fully interruptible -- interrupts are already enabled
        // above, so a test IPI is handled without disturbing this loop.
        let slot = percpu::slot(core_index);
        loop {
            slot.spin_count.fetch_add(1, Ordering::Relaxed);
            core::hint::spin_loop();
        }
    }
    #[cfg(not(feature = "smp-boot-test"))]
    loop {
        x86_64::instructions::hlt();
    }
}

/// Enumerates every CPU Limine reported (via `resp`, from `main.rs`'s
/// `MP_REQUEST`) and starts every one that isn't the BSP itself, then
/// waits — with a bounded, logged timeout, never forever — for each
/// started core to report itself ready. Must run after
/// `arch::x86_64::init()` (needs the BSP's own GDT/IDT/LAPIC already
/// set up) and before anything spawns a process, since `Process` switch
/// paths resolve "which core is this" via `percpu::core_index()`, which
/// requires this function's own `percpu::assign_slot(0, ...)` call for
/// the BSP to have already run.
/// Fallback for the (in practice, never-expected-but-handled-anyway)
/// case where Limine didn't honor `MP_REQUEST` at all: registers the BSP
/// as core 0 via its own `CPUID`-read APIC ID instead of
/// `MpRespData::bsp_lapic_id`, so `percpu::core_index()` still resolves
/// correctly even with no additional cores ever started.
pub fn bring_up_bsp_only() {
    percpu::assign_slot(0, percpu::read_own_apic_id());
    percpu::slot(0).ready.store(true, Ordering::Release);
    earlyprintln!("[smp] MP_REQUEST not honored -- running BSP-only");
}

pub fn bring_up_aps(resp: &MpRespData) {
    percpu::assign_slot(0, resp.bsp_lapic_id);
    percpu::slot(0).ready.store(true, Ordering::Release);
    // Mirrors every AP's own self-announcement (`ap_entry_on_own_stack`)
    // so `xtask test-smp-boot`'s "one ready line per reported CPU"
    // check counts the BSP too, not just the APs it starts below.
    earlyprintln!("[smp] core 0 ready");

    // Map every possible core's idle stack up front, eagerly, before any
    // AP is started -- see `idle_stack_top`'s doc comment.
    for core_index in 0..percpu::MAX_CORES {
        idle_stack_top(core_index);
    }

    let mut started = 1usize; // slot 0 (the BSP) always counts as started.
    for info in resp.cpus() {
        if info.lapic_id == resp.bsp_lapic_id {
            continue;
        }
        if started >= percpu::MAX_CORES {
            earlyprintln!(
                "[smp] Limine reported more CPUs than MAX_CORES ({}) -- ignoring the rest",
                percpu::MAX_CORES
            );
            break;
        }
        percpu::assign_slot(started, info.lapic_id);
        let goto: MpGotoFunction = ap_entry_trampoline;
        info.bootstrap(goto, started as u64);
        started += 1;
    }

    // Bounded wait: an AP that never reports ready (a genuine bring-up
    // failure, or the undocumented landing-state gap this module exists
    // to guard against) must not hang the BSP forever.
    const TIMEOUT_SPINS: u64 = 100_000_000;
    for core_index in 1..started {
        let mut spins = 0u64;
        while !percpu::slot(core_index).ready.load(Ordering::Acquire) {
            core::hint::spin_loop();
            spins += 1;
            if spins > TIMEOUT_SPINS {
                earlyprintln!(
                    "[smp] core {} did not report ready in time -- continuing without it",
                    core_index
                );
                break;
            }
        }
    }

    earlyprintln!("[smp] bring-up complete: {} core(s) started", started);
}
