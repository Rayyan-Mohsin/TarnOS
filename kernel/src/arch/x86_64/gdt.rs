//! Global Descriptor Table + Task State Segment.
//!
//! Limine hands off with its own temporary GDT; this module replaces it
//! with TarnOS's own, laid out so it can be reused unchanged once SYSCALL/
//! SYSRET is wired up (a later milestone task): the x86_64 SYSCALL/SYSRET
//! architecture hard-codes segment selectors as fixed offsets from two MSR
//! base values, which only works if kernel_data sits exactly one GDT slot
//! after kernel_code, and user_code exactly one slot after user_data. That
//! ordering is set up now so it never has to be revisited.
//!
//! One shared `GlobalDescriptorTable`, not one per core: every core's
//! CS/SS/DS/ES selectors stay numerically identical, and only the
//! `ltr`-loaded TSS selector differs per core — see [`TSS_TABLE`].
use core::cell::UnsafeCell;

use spin::Once;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::paging::{FrameAllocator, Page, PageTableFlags, Size4KiB};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

use super::percpu::{self, MAX_CORES};
use crate::memory::phys::GlobalFrameAllocator;
use crate::memory::virt;

/// Index into the TSS's Interrupt Stack Table reserved for double faults.
///
/// A double fault can be caused by the kernel's own stack being invalid
/// (e.g. a stack overflow triggering a page fault while the CPU is trying
/// to push the page-fault frame onto that same broken stack). The IST lets
/// the CPU switch to a known-good stack purely from GDT/TSS state, before
/// any kernel code runs — the one case where a "just use the current
/// stack" handler cannot be made to work.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

const DOUBLE_FAULT_STACK_PAGES: u64 = 5;
/// Fixed virtual base for the double-fault stack region, chosen clear of
/// the kernel heap (`0xffff_9000_0000_0000`) and the per-process kernel
/// stack region (`0xffff_9800_0000_0000`, see `task::process`). One slot
/// per possible core — see [`double_fault_stack_slot_base`] — since two
/// cores double-faulting at once must not corrupt each other's exception
/// stack.
const DOUBLE_FAULT_STACK_BASE: u64 = 0xffff_9400_0000_0000;
/// Guard page + stack, one slot per core index — same shape as
/// `task::process::KERNEL_STACK_SLOT_STRIDE`.
const DOUBLE_FAULT_STACK_SLOT_STRIDE: u64 = 4096 * (1 + DOUBLE_FAULT_STACK_PAGES);

pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
    /// One TSS selector per core index — `tss[i]` is only ever `ltr`'d by
    /// core `i` itself (in [`init_bsp`] for core 0, [`init_ap`] for every
    /// other core).
    tss: [SegmentSelector; MAX_CORES],
}

/// Wraps a TSS in an `UnsafeCell` rather than `spin::Once`'s plain value:
/// `set_kernel_stack` needs to keep mutating `privilege_stack_table[0]`
/// (RSP0) on every process switch, long after [`init_bsp`] has run, while
/// the GDT's TSS descriptor holds a `&'static` pointing at this same
/// memory. Safe because every mutation of a given core's own `TssCell`
/// happens with interrupts disabled on that same core (from within
/// `sync::SpinLock`-guarded scheduler code), and no other core ever
/// touches a `TssCell` that isn't its own.
struct TssCell(UnsafeCell<TaskStateSegment>);
unsafe impl Sync for TssCell {}

/// One TSS per possible core — see [`Selectors::tss`] and the module doc
/// comment for why the GDT itself stays a single shared table while the
/// TSS does not.
static TSS_TABLE: [TssCell; MAX_CORES] =
    [const { TssCell(UnsafeCell::new(TaskStateSegment::new())) }; MAX_CORES];

/// A TSS descriptor is a "system segment" and occupies *two* GDT entries,
/// not one (`x86_64::structures::gdt::GlobalDescriptorTable::append`'s
/// own doc comment: "depending on the type of the `Descriptor` this may
/// append either one or two new `Entry`s"). The default `MAX = 8` a bare
/// `GlobalDescriptorTable::new()` gives only has room for the null entry,
/// the four fixed segments below, and two TSS descriptors — nowhere near
/// `MAX_CORES` of them — so this GDT must be sized explicitly: 1 (null)
/// + 4 (kernel/user code/data) + 2 per possible core's TSS descriptor.
const GDT_ENTRIES: usize = 1 + 4 + 2 * MAX_CORES;

static GDT: Once<(GlobalDescriptorTable<GDT_ENTRIES>, Selectors)> = Once::new();
static SELECTORS: Once<&'static Selectors> = Once::new();

fn double_fault_stack_slot_base(core_index: usize) -> u64 {
    DOUBLE_FAULT_STACK_BASE + core_index as u64 * DOUBLE_FAULT_STACK_SLOT_STRIDE
}

/// Maps [`DOUBLE_FAULT_STACK_PAGES`] pages at `core_index`'s fixed
/// virtual slot and returns the top of that mapping — deliberately
/// leaving the page immediately below it unmapped as a guard page. A
/// double fault is exactly the case a stack overflow can trigger (the CPU
/// faulting again while trying to push an exception frame onto an
/// already-exhausted stack), so this is the one stack in the kernel where
/// an unguarded overflow would be most likely to silently corrupt
/// whatever memory happened to sit below it instead of reliably faulting.
///
/// Must run after `memory::init()` (needs the frame allocator and page
/// mapper) — see the boot-order note in `main.rs`. Called for every
/// possible core index eagerly, up front, by [`init_bsp`] — an AP cannot
/// safely map its own stack before it has even verified its own landing
/// state (see `arch::x86_64::smp`), so the BSP does this for all
/// `MAX_CORES` slots before any AP ever starts, the same "eager, for
/// every possible slot, once" discipline
/// `task::process::init_kernel_stacks` already uses.
fn double_fault_stack_top(core_index: usize) -> VirtAddr {
    let mut allocator = GlobalFrameAllocator;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    let slot_base = double_fault_stack_slot_base(core_index);
    for i in 1..=DOUBLE_FAULT_STACK_PAGES {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(slot_base + i * 4096));
        let frame = allocator
            .allocate_frame()
            .expect("out of memory mapping a double-fault stack");
        virt::map(page, frame, flags).expect("failed to map double-fault stack page");
    }
    VirtAddr::new(slot_base + (1 + DOUBLE_FAULT_STACK_PAGES) * 4096)
}

/// Builds the shared GDT (with `MAX_CORES` TSS descriptors, one per
/// possible core) and finishes bring-up for the BSP itself (core index
/// 0): loads the GDT, sets CS/SS/DS/ES, and `ltr`s core 0's own TSS
/// selector. Must run before any AP is started — [`init_ap`] assumes the
/// GDT this function builds already exists.
pub fn init_bsp() {
    // SAFETY: single-threaded boot, before any AP starts and before
    // interrupts are enabled.
    for (core_index, tss_cell) in TSS_TABLE.iter().enumerate() {
        let tss: &'static mut TaskStateSegment = unsafe { &mut *tss_cell.0.get() };
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            double_fault_stack_top(core_index);
    }

    let (gdt, selectors) = GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::<GDT_ENTRIES>::empty();
        // Order matters: see the module doc comment.
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());

        let mut tss = [SegmentSelector(0); MAX_CORES];
        for (core_index, slot) in tss.iter_mut().enumerate() {
            // SAFETY: `TSS_TABLE[core_index]` is 'static and never moves;
            // this shared reference is only ever used to build its GDT
            // descriptor, never to read/write the TSS's fields.
            let tss_ref: &'static TaskStateSegment =
                unsafe { &*TSS_TABLE[core_index].0.get() };
            *slot = gdt.append(Descriptor::tss_segment(tss_ref));
        }

        (
            gdt,
            Selectors {
                kernel_code,
                kernel_data,
                user_data,
                user_code,
                tss,
            },
        )
    });

    gdt.load();
    unsafe {
        CS::set_reg(selectors.kernel_code);
        SS::set_reg(selectors.kernel_data);
        DS::set_reg(selectors.kernel_data);
        ES::set_reg(selectors.kernel_data);
        load_tss(selectors.tss[0]);
    }

    SELECTORS.call_once(|| selectors);
}

/// Finishes GDT/TSS bring-up for an additional core: no GDT *rebuild*
/// (the content [`init_bsp`] already built is shared and read-only from
/// here on — the same reasoning `idt::load_ap` relies on for the IDT),
/// but every core still has its own GDTR and must `lgdt` for itself
/// before any segment-register reload can work — skipping this left an
/// AP still running under Limine's own temporary GDT, so `CS::set_reg`'s
/// far-return (below) faulted on a selector that didn't exist in
/// *that* table, and with `idt::load_ap` not yet called either, that
/// fault had nowhere valid to go and silently triple-faulted the core
/// instead of producing any diagnostic — caught by this exact
/// milestone's own `xtask test-smp-boot` hanging with zero "ready"
/// lines. After `lgdt`, this does what's genuinely per-core: loading the
/// same selectors into this core's own segment registers, and `ltr`-ing
/// *this* core's own TSS selector.
///
/// # Safety
/// Must only be called after [`init_bsp`] has completed, and only once,
/// by the core whose `core_index` is passed in.
pub unsafe fn init_ap(core_index: usize) {
    let (gdt, selectors) = GDT
        .get()
        .expect("gdt::init_bsp() must run before gdt::init_ap()");
    gdt.load();
    unsafe {
        CS::set_reg(selectors.kernel_code);
        SS::set_reg(selectors.kernel_data);
        DS::set_reg(selectors.kernel_data);
        ES::set_reg(selectors.kernel_data);
        load_tss(selectors.tss[core_index]);
    }
}

/// The segment selectors chosen at [`init_bsp`] time, needed later to
/// program the `STAR` MSR for SYSCALL/SYSRET.
pub fn selectors() -> &'static Selectors {
    SELECTORS
        .get()
        .expect("gdt::init_bsp() must run before gdt::selectors()")
}

/// Sets RSP0 — the kernel stack the CPU switches to on any trap
/// (interrupt, exception, or `SYSCALL`) that raises the privilege level —
/// for *this calling core's own* TSS. Called on every process switch so a
/// trap taken while a given process is running always lands on that
/// process's kernel stack, never a different process's.
///
/// Resolves "which core is calling me" via `percpu::core_index()` rather
/// than taking an explicit core index: this milestone never runs a
/// process anywhere but the BSP, so every caller today is implicitly
/// core 0, but writing it this way means it's already correct for a
/// future milestone that schedules processes on other cores too, with no
/// change needed here or at any call site.
///
/// # Safety
/// Must only be called with interrupts disabled — the caller is
/// overwriting state the CPU consults on the next privilege-raising trap,
/// which must not happen mid-update.
pub unsafe fn set_kernel_stack(rsp0: VirtAddr) {
    let core_index = percpu::core_index();
    unsafe {
        (*TSS_TABLE[core_index].0.get()).privilege_stack_table[0] = rsp0;
    }
}
