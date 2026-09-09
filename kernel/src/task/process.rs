//! A process: an address space, a saved CPU state, a kernel stack to trap
//! into, and the capabilities it holds.
use alloc::vec;
use alloc::boxed::Box;

use x86_64::structures::paging::{FrameAllocator, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

use crate::arch::x86_64::context_switch::TrapFrame;
use crate::ipc::CapTable;
use crate::memory::phys::GlobalFrameAllocator;
use crate::memory::virt::{self, AddressSpace};

use super::Pid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Ready,
    Running,
}

const KERNEL_STACK_SIZE: usize = 16 * 1024;
const USER_STACK_PAGES: u64 = 4; // 16 KiB
/// Just under the top of the canonical lower half — the same permitted
/// range `elf::load` validates PT_LOAD segments against — leaving a
/// large gap below it for a future heap/mmap region.
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;

pub struct Process {
    pub pid: Pid,
    pub address_space: AddressSpace,
    pub trap_frame: TrapFrame,
    /// Boxed as a slice (not an inline array) specifically so
    /// construction never materializes 16 KiB on the *current* stack
    /// before moving it to the heap — `vec![0; N]` writes straight into
    /// the new heap allocation.
    kernel_stack: Box<[u8]>,
    pub cap_table: CapTable,
    pub state: ProcessState,
}

impl Process {
    pub fn kernel_stack_top(&self) -> VirtAddr {
        VirtAddr::from_ptr(self.kernel_stack.as_ptr()) + self.kernel_stack.len() as u64
    }

    /// Core constructor: an address space plus an entry point becomes a
    /// process ready to run, with a freshly mapped user stack. Both this
    /// milestone's dummy-process smoke test ([`Process::new_dummy`]) and
    /// a real ELF-loaded process (wired up in a later milestone task)
    /// build on this one path — there is exactly one way a process's
    /// initial state gets constructed.
    fn new(pid: Pid, address_space: AddressSpace, entry: VirtAddr) -> Result<Self, &'static str> {
        let mut allocator = GlobalFrameAllocator;
        let stack_top = VirtAddr::new(USER_STACK_TOP);
        let stack_bottom = VirtAddr::new(USER_STACK_TOP - USER_STACK_PAGES * 4096);
        let start_page = Page::<Size4KiB>::containing_address(stack_bottom);
        let end_page = Page::<Size4KiB>::containing_address(stack_top - 1u64);
        for page in Page::range_inclusive(start_page, end_page) {
            let frame = allocator
                .allocate_frame()
                .ok_or("out of memory mapping user stack")?;
            let flags = PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::USER_ACCESSIBLE
                | PageTableFlags::NO_EXECUTE;
            address_space
                .map(page, frame, flags)
                .map_err(|_| "failed to map user stack")?;
        }

        let trap_frame = TrapFrame::initial_user_frame(entry, stack_top);

        Ok(Self {
            pid,
            address_space,
            trap_frame,
            kernel_stack: vec![0u8; KERNEL_STACK_SIZE].into_boxed_slice(),
            cap_table: CapTable::new(),
            state: ProcessState::Ready,
        })
    }

    /// Builds a process by remapping an existing kernel-compiled
    /// function's code page as user-executable in a fresh address space,
    /// standing in for a real ELF-loaded process before the boot
    /// sequence loads one (a later milestone task). Validates the
    /// scheduler and context-switch machinery in isolation from ELF and
    /// syscall complexity, per the milestone plan.
    ///
    /// `entry_fn` must be a short, self-contained function — its code
    /// must not cross a page boundary, since only the single page
    /// containing its start address is remapped.
    pub fn new_dummy(pid: Pid, entry_fn: unsafe extern "C" fn() -> !) -> Result<Self, &'static str> {
        let address_space = AddressSpace::new().map_err(|_| "out of memory creating address space")?;

        let kernel_vaddr = VirtAddr::new(entry_fn as *const () as u64);
        let phys = virt::translate(kernel_vaddr).ok_or("dummy entry function is not mapped")?;
        let frame = PhysFrame::<Size4KiB>::containing_address(phys);
        let page_offset = phys.as_u64() - frame.start_address().as_u64();

        const DUMMY_CODE_BASE: u64 = 0x0000_0000_0040_0000;
        let user_code_page = Page::<Size4KiB>::containing_address(VirtAddr::new(DUMMY_CODE_BASE));
        address_space
            .map(
                user_code_page,
                frame,
                PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE,
            )
            .map_err(|_| "failed to map dummy code page")?;

        let entry = VirtAddr::new(DUMMY_CODE_BASE + page_offset);
        Self::new(pid, address_space, entry)
    }
}
