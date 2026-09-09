//! Global Descriptor Table + Task State Segment.
//!
//! Limine hands off with its own temporary GDT; this module replaces it
//! with TarnOS's own, laid out so it can be reused unchanged once SYSCALL/
//! SYSRET is wired up (a later milestone task): the x86_64 SYSCALL/SYSRET
//! architecture hard-codes segment selectors as fixed offsets from two MSR
//! base values, which only works if kernel_data sits exactly one GDT slot
//! after kernel_code, and user_code exactly one slot after user_data. That
//! ordering is set up now so it never has to be revisited.
use core::cell::UnsafeCell;

use spin::Once;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::paging::{FrameAllocator, Page, PageTableFlags, Size4KiB};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

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
/// Fixed virtual base for the double-fault stack, chosen clear of the
/// kernel heap (`0xffff_9000_0000_0000`) and the per-process kernel
/// stack region (`0xffff_9800_0000_0000`, see `task::process`).
const DOUBLE_FAULT_STACK_BASE: u64 = 0xffff_9400_0000_0000;

pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub tss: SegmentSelector,
}

/// Wraps the TSS in an `UnsafeCell` rather than `spin::Once`'s plain
/// value: `set_kernel_stack` needs to keep mutating `privilege_stack_table[0]`
/// (RSP0) on every process switch, long after `init` has run, while the
/// GDT's TSS descriptor holds a `&'static` pointing at this same memory.
/// Safe because every mutation happens with interrupts disabled (from
/// within `sync::SpinLock`-guarded scheduler code) on this single core.
struct TssCell(UnsafeCell<TaskStateSegment>);
unsafe impl Sync for TssCell {}

static TSS: TssCell = TssCell(UnsafeCell::new(TaskStateSegment::new()));
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();
static SELECTORS: Once<&'static Selectors> = Once::new();

/// Maps [`DOUBLE_FAULT_STACK_PAGES`] pages at a fixed virtual base and
/// returns the top of that mapping — deliberately leaving the page
/// immediately below it (at `DOUBLE_FAULT_STACK_BASE` itself) unmapped as
/// a guard page. A double fault is exactly the case a stack overflow can
/// trigger (the CPU faulting again while trying to push an exception
/// frame onto an already-exhausted stack), so this is the one stack in
/// the kernel where an unguarded overflow would be most likely to
/// silently corrupt whatever memory happened to sit below it instead of
/// reliably faulting.
///
/// Must run after `memory::init()` (needs the frame allocator and page
/// mapper) — see the boot-order note in `main.rs`.
fn double_fault_stack_top() -> VirtAddr {
    let mut allocator = GlobalFrameAllocator;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    for i in 1..=DOUBLE_FAULT_STACK_PAGES {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(
            DOUBLE_FAULT_STACK_BASE + i * 4096,
        ));
        let frame = allocator
            .allocate_frame()
            .expect("out of memory mapping the double-fault stack");
        virt::map(page, frame, flags).expect("failed to map double-fault stack page");
    }
    VirtAddr::new(DOUBLE_FAULT_STACK_BASE + (1 + DOUBLE_FAULT_STACK_PAGES) * 4096)
}

pub fn init() {
    // SAFETY: single-threaded boot, before interrupts are enabled.
    let tss: &'static mut TaskStateSegment = unsafe { &mut *TSS.0.get() };
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = double_fault_stack_top();
    let tss: &'static TaskStateSegment = tss;

    let (gdt, selectors) = GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        // Order matters: see the module doc comment.
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(tss));
        (
            gdt,
            Selectors {
                kernel_code,
                kernel_data,
                user_data,
                user_code,
                tss: tss_sel,
            },
        )
    });

    gdt.load();
    unsafe {
        CS::set_reg(selectors.kernel_code);
        SS::set_reg(selectors.kernel_data);
        DS::set_reg(selectors.kernel_data);
        ES::set_reg(selectors.kernel_data);
        load_tss(selectors.tss);
    }

    SELECTORS.call_once(|| selectors);
}

/// The segment selectors chosen at [`init`] time, needed later to program
/// the `STAR` MSR for SYSCALL/SYSRET.
pub fn selectors() -> &'static Selectors {
    SELECTORS
        .get()
        .expect("gdt::init() must run before gdt::selectors()")
}

/// Sets RSP0: the kernel stack the CPU switches to on any trap
/// (interrupt, exception, or `SYSCALL`) that raises the privilege level.
/// Called on every process switch so a trap taken while a given process
/// is running always lands on *that* process's kernel stack, never a
/// different process's.
///
/// # Safety
/// Must only be called with interrupts disabled — the caller is
/// overwriting state the CPU consults on the next privilege-raising trap,
/// which must not happen mid-update.
pub unsafe fn set_kernel_stack(rsp0: VirtAddr) {
    unsafe {
        (*TSS.0.get()).privilege_stack_table[0] = rsp0;
    }
}
