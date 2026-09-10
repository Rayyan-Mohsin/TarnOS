//! A process: an address space, a saved CPU state, a kernel stack to trap
//! into, and the capabilities it holds.
use x86_64::structures::paging::{FrameAllocator, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

use crate::arch::x86_64::context_switch::TrapFrame;
use crate::ipc::CapTable;
use crate::memory::phys::GlobalFrameAllocator;
use crate::memory::virt::{self, AddressSpace};

use super::scheduler::MAX_PROCESSES;
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
/// (`0xffff_9400_0000_0000`, see `arch::x86_64::gdt`).
///
/// Mapped into the *shared* kernel half via [`init_kernel_stacks`] —
/// eagerly, for every one of the `MAX_PROCESSES` possible slots, once at
/// boot, before any process's own `AddressSpace` is ever created —
/// rather than lazily into each process's own address space the way an
/// earlier version of this milestone did it. That first attempt looked
/// reasonable (a stack is "owned" by one process, only ever touched
/// while that process's CR3 is active) but was wrong in a way that only
/// showed up once a second real process existed to switch to: the code
/// that performs a switch — `task::scheduler::switch_to` and its callers
/// — is still executing *on the outgoing process's own kernel stack*
/// for a while after `AddressSpace::activate()` changes CR3. If that
/// stack only existed in the outgoing process's own page tables, the
/// very next stack access after the switch (a `push`, a local variable,
/// the next `ret`) page-faults, because the address the CPU's RSP
/// already points at just became unmapped out from under it — observed
/// in practice as a double fault immediately after a fault handler
/// killed one process and tried to switch to another. Mapping every
/// slot into the shared kernel half up front — exactly like the heap
/// and the double-fault stack — means every process's page tables see
/// every stack identically, so a CR3 switch can never make the
/// currently-in-use stack disappear.
const KERNEL_STACKS_BASE: u64 = 0xffff_9800_0000_0000;
const KERNEL_STACK_SLOT_STRIDE: u64 = 4096 * (1 + KERNEL_STACK_PAGES); // guard page + stack

fn kernel_stack_slot_base(pid: Pid) -> u64 {
    KERNEL_STACKS_BASE + pid.0 * KERNEL_STACK_SLOT_STRIDE
}

/// Maps every possible process's kernel stack (with its guard page) into
/// the shared kernel half. Must run once, after `memory::init()` and
/// before the first `AddressSpace::new()` call — see the module-level
/// doc comment on [`KERNEL_STACKS_BASE`] for why eager and shared,
/// rather than lazy and per-process, is load-bearing here.
pub fn init_kernel_stacks() {
    let mut allocator = GlobalFrameAllocator;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    for slot in 0..MAX_PROCESSES as u64 {
        let slot_base = KERNEL_STACKS_BASE + slot * KERNEL_STACK_SLOT_STRIDE;
        for i in 1..=KERNEL_STACK_PAGES {
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(slot_base + i * 4096));
            let frame = allocator
                .allocate_frame()
                .expect("out of memory mapping kernel stacks");
            virt::map(page, frame, flags).expect("failed to map a kernel stack page");
        }
    }
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
