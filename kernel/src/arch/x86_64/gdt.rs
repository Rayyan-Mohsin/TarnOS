//! Global Descriptor Table + Task State Segment.
//!
//! Limine hands off with its own temporary GDT; this module replaces it
//! with TarnOS's own, laid out so it can be reused unchanged once SYSCALL/
//! SYSRET is wired up (a later milestone task): the x86_64 SYSCALL/SYSRET
//! architecture hard-codes segment selectors as fixed offsets from two MSR
//! base values, which only works if kernel_data sits exactly one GDT slot
//! after kernel_code, and user_code exactly one slot after user_data. That
//! ordering is set up now so it never has to be revisited.
use spin::Once;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// Index into the TSS's Interrupt Stack Table reserved for double faults.
///
/// A double fault can be caused by the kernel's own stack being invalid
/// (e.g. a stack overflow triggering a page fault while the CPU is trying
/// to push the page-fault frame onto that same broken stack). The IST lets
/// the CPU switch to a known-good stack purely from GDT/TSS state, before
/// any kernel code runs — the one case where a "just use the current
/// stack" handler cannot be made to work.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

const DOUBLE_FAULT_STACK_SIZE: usize = 4096 * 5;

pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub tss: SegmentSelector,
}

static TSS: Once<TaskStateSegment> = Once::new();
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();
static SELECTORS: Once<&'static Selectors> = Once::new();

fn double_fault_stack_top() -> VirtAddr {
    static mut STACK: [u8; DOUBLE_FAULT_STACK_SIZE] = [0; DOUBLE_FAULT_STACK_SIZE];
    let start = VirtAddr::from_ptr(&raw const STACK);
    start + DOUBLE_FAULT_STACK_SIZE as u64
}

pub fn init() {
    let tss = TSS.call_once(|| {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = double_fault_stack_top();
        tss
    });

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
