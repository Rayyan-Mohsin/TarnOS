//! A process: an address space, a saved CPU state, a kernel stack to trap
//! into, and the capabilities it holds.
use x86_64::structures::paging::{FrameAllocator, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

use crate::arch::x86_64::context_switch::TrapFrame;
use crate::ipc::CapTable;
use crate::memory::phys::GlobalFrameAllocator;
use crate::memory::virt::{self, AddressSpace, MapError};

use super::Pid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Ready,
    Running,
    /// Suspended inside `SYS_SEND`/`SYS_RECV` with no partner ready —
    /// deliberately not in the scheduler's ready queue (that's what
    /// distinguishes this from an ordinary preemption) until
    /// `task::scheduler::wake_blocked_process` moves it back to `Ready`.
    Blocked,
}

const KERNEL_STACK_PAGES: u64 = 4; // 16 KiB
const USER_STACK_PAGES: u64 = 4; // 16 KiB
/// Just under the top of the canonical lower half — the same permitted
/// range `elf::load` validates PT_LOAD segments against — leaving a
/// large gap below it for a future heap/mmap region.
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;

/// Fixed virtual base for per-process kernel stacks, one
/// `KERNEL_STACK_SLOT_STRIDE`-sized slot per `Pid` — chosen clear of the
/// kernel heap (`0xffff_9000_0000_0000`) and the double-fault stack
/// (`0xffff_9400_0000_0000`, see `arch::x86_64::gdt`). Mapped through
/// each process's own `AddressSpace` rather than carved from the heap
/// (as earlier milestones did with a plain `Box<[u8]>`) specifically so
/// a guard page can sit immediately below every stack: a kernel-stack
/// overflow reliably page-faults instead of silently corrupting whatever
/// heap allocation happened to land next to it. Never accessed except
/// while its owning process's address space is active (see
/// `task::scheduler::switch_to`), so — unlike the heap — this mapping
/// only ever needs to exist in that one process's own page tables, not
/// shared with anyone else's.
const KERNEL_STACKS_BASE: u64 = 0xffff_9800_0000_0000;
const KERNEL_STACK_SLOT_STRIDE: u64 = 4096 * (1 + KERNEL_STACK_PAGES); // guard page + stack

fn kernel_stack_slot_base(pid: Pid) -> u64 {
    KERNEL_STACKS_BASE + pid.0 * KERNEL_STACK_SLOT_STRIDE
}

/// Maps this process's kernel stack pages into `address_space`, leaving
/// the page at the slot's base address unmapped as a guard page.
fn map_kernel_stack(pid: Pid, address_space: &AddressSpace) -> Result<(), MapError> {
    let mut allocator = GlobalFrameAllocator;
    let slot_base = kernel_stack_slot_base(pid);
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    for i in 1..=KERNEL_STACK_PAGES {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(slot_base + i * 4096));
        let frame = allocator.allocate_frame().ok_or(MapError::OutOfMemory)?;
        address_space.map(page, frame, flags)?;
    }
    Ok(())
}

pub struct Process {
    pub pid: Pid,
    pub address_space: AddressSpace,
    pub trap_frame: TrapFrame,
    pub cap_table: CapTable,
    pub state: ProcessState,
}

impl Process {
    pub fn kernel_stack_top(&self) -> VirtAddr {
        VirtAddr::new(kernel_stack_slot_base(self.pid) + (1 + KERNEL_STACK_PAGES) * 4096)
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

        map_kernel_stack(pid, &address_space).map_err(|_| "failed to map kernel stack")?;

        let trap_frame = TrapFrame::initial_user_frame(entry, stack_top);

        Ok(Self {
            pid,
            address_space,
            trap_frame,
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

    /// Builds a process by loading a real static ELF64 `ET_EXEC` image —
    /// the actual way a process is created, once there's an ELF to load;
    /// `new_dummy` above only exists because this milestone bootstraps
    /// scheduler/syscall validation before `init`'s ELF bytes are wired
    /// up as a boot module.
    pub fn from_elf(pid: Pid, elf_bytes: &[u8]) -> Result<Self, &'static str> {
        let address_space =
            AddressSpace::new().map_err(|_| "out of memory creating address space")?;
        let mut allocator = GlobalFrameAllocator;
        let entry = {
            let mut mapper = unsafe { address_space.mapper() };
            crate::elf::load(elf_bytes, &mut mapper, &mut allocator)
                .map_err(|_| "failed to load ELF image")?
                .entry
        };
        Self::new(pid, address_space, entry)
    }
}
