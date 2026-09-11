//! A process: an address space, a saved CPU state, a kernel stack to trap
//! into, and the capabilities it holds.
extern crate alloc;

use alloc::vec::Vec;

use spin::Once;
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
    /// Created by `SYS_SPAWN`, not yet in the scheduler's ready queue —
    /// invisible to the scheduler until `SYS_PROCESS_START` releases it.
    /// While in this state, only its `parent` may touch its capability
    /// table (`SYS_GRANT`) or release it (`SYS_PROCESS_START`) — see
    /// `task::scheduler::with_process` and
    /// `docs/adr/0006-dynamic-process-creation-and-capability-transfer.md`.
    /// Once released, the child is an ordinary independent process; the
    /// parent relationship confers no further authority.
    Suspended,
}

const KERNEL_STACK_PAGES: u64 = 4; // 16 KiB
const USER_STACK_PAGES: u64 = 4; // 16 KiB
/// Just under the top of the canonical lower half.
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;

/// Fixed virtual base for every process's heap — 1 GiB, comfortably
/// above where any current or near-term `tarnos-rt`-linked binary's
/// `PT_LOAD` segments land (`elf::load` now rejects a segment that would
/// reach this high — see `elf.rs`), and comfortably below the user stack
/// at [`USER_STACK_TOP`]. `sys_sbrk` is the only thing that ever moves a
/// process's break above this address; see `Process::heap_end`.
pub const USER_HEAP_START: u64 = 0x0000_0000_4000_0000;

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
    KERNEL_STACKS_BASE + pid.index() as u64 * KERNEL_STACK_SLOT_STRIDE
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
    /// The process that created this one via `SYS_SPAWN`, if any —
    /// `None` for every process boot code creates directly. While
    /// `state == Suspended`, it's the one process permitted to grant
    /// capabilities into this one or release it (see
    /// `task::scheduler::with_process`/`start_child`). It remains
    /// authoritative for the rest of this process's life for `SYS_KILL`/
    /// `SYS_WAIT` (see `docs/adr/0007-process-lifecycle-and-termination.md`),
    /// even though it confers no further *grant/start* authority once
    /// the child leaves `Suspended`.
    pub parent: Option<Pid>,
    /// Set by `SYS_WAIT` when some other process — always `parent`,
    /// since only `parent` is ever permitted to wait on this one — is
    /// currently blocked waiting for this process to terminate. Since
    /// at most one process can ever legitimately hold that role, this
    /// is a single field, not a queue; see
    /// `task::scheduler::wait_for_child`/`take_and_finalize_slot`.
    pub wait_waiter: Option<Pid>,
    /// The current end of this process's heap — its `sys_sbrk` break.
    /// Starts at [`USER_HEAP_START`] (an empty heap) and only ever grows
    /// (see `arch::x86_64::syscall::sys_sbrk` — shrinking is not
    /// supported this milestone). Pages between `USER_HEAP_START` and
    /// this address, rounded up to the containing page, are mapped into
    /// `address_space`; nothing beyond that rounded-up page is ever
    /// mapped ahead of need.
    pub heap_end: u64,
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
    fn new(
        pid: Pid,
        address_space: AddressSpace,
        entry: VirtAddr,
        parent: Option<Pid>,
    ) -> Result<Self, &'static str> {
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
            parent,
            wait_waiter: None,
            heap_end: USER_HEAP_START,
        })
    }

    /// Builds a process by copying an existing kernel-compiled
    /// function's machine code into fresh, process-owned pages in a new
    /// address space, standing in for a real ELF-loaded process before
    /// the boot sequence loads one (a later milestone task). Validates
    /// the scheduler and context-switch machinery in isolation from ELF
    /// and syscall complexity, per the milestone plan.
    ///
    /// Copies rather than remaps the kernel's own `.text` frame
    /// directly: `AddressSpace::drop` frees every leaf frame it finds
    /// mapped when a process's address space is torn down (see
    /// `memory::virt`), so remapping a live kernel code frame into a
    /// process would make that process's exit incorrectly free running
    /// kernel code back to the allocator — silent corruption the next
    /// time that frame got handed out for something else. Copying gives
    /// the process its own frames, safe to free like any other.
    ///
    /// `entry_fn` must be a short, self-contained function, comfortably
    /// under one page in compiled size — two consecutive pages are
    /// copied (see below) specifically because a function's start
    /// offset within its own original page is whatever the compiler and
    /// linker happened to place it at, never guaranteed to be
    /// page-aligned, so its tail can spill into the following page for
    /// any function longer than the room remaining from that offset.
    pub fn new_dummy(
        pid: Pid,
        entry_fn: unsafe extern "C" fn() -> !,
        parent: Option<Pid>,
    ) -> Result<Self, &'static str> {
        let address_space = AddressSpace::new().map_err(|_| "out of memory creating address space")?;
        let mut allocator = GlobalFrameAllocator;

        let kernel_vaddr = VirtAddr::new(entry_fn as *const () as u64);
        let phys = virt::translate(kernel_vaddr).ok_or("dummy entry function is not mapped")?;
        let source_frame = PhysFrame::<Size4KiB>::containing_address(phys);
        let page_offset = phys.as_u64() - source_frame.start_address().as_u64();

        const DUMMY_CODE_BASE: u64 = 0x0000_0000_0040_0000;
        let flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;

        // Two pages (see this function's doc comment for why), each
        // copied byte-for-byte from the kernel's own compiled code at
        // the same relative offset, so the function's internal
        // (PC-relative) jumps and its `const`-baked-as-immediate
        // operands all remain correct at the new address.
        for i in 0..2u64 {
            let dest_frame = allocator
                .allocate_frame()
                .ok_or("out of memory copying dummy code")?;
            // SAFETY: both addresses are HHDM aliases of real physical
            // frames — `source_frame + i` is still-live kernel code,
            // `dest_frame` was just allocated (so exclusively ours) —
            // and neither range overlaps the other.
            unsafe {
                let source = virt::phys_to_virt(source_frame.start_address() + i * 4096).as_ptr::<u8>();
                let dest = virt::phys_to_virt(dest_frame.start_address()).as_mut_ptr::<u8>();
                core::ptr::copy_nonoverlapping(source, dest, 4096);
            }

            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(DUMMY_CODE_BASE + i * 4096));
            address_space
                .map(page, dest_frame, flags)
                .map_err(|_| "failed to map dummy code page")?;
        }

        let entry = VirtAddr::new(DUMMY_CODE_BASE + page_offset);
        Self::new(pid, address_space, entry, parent)
    }

    /// Builds a process by loading a real static ELF64 `ET_EXEC` image —
    /// the actual way a process is created, once there's an ELF to load;
    /// `new_dummy` above only exists because this milestone bootstraps
    /// scheduler/syscall validation before `init`'s ELF bytes are wired
    /// up as a boot module.
    ///
    /// `parent` is `None` for a process boot code creates directly (e.g.
    /// `init`), or `Some(caller_pid)` when created via `SYS_SPAWN` —
    /// see [`ProcessState::Suspended`].
    pub fn from_elf(pid: Pid, elf_bytes: &[u8], parent: Option<Pid>) -> Result<Self, &'static str> {
        let address_space =
            AddressSpace::new().map_err(|_| "out of memory creating address space")?;
        let mut allocator = GlobalFrameAllocator;
        let entry = {
            let mut mapper = unsafe { address_space.mapper() };
            crate::elf::load(elf_bytes, &mut mapper, &mut allocator)
                .map_err(|_| "failed to load ELF image")?
                .entry
        };
        Self::new(pid, address_space, entry, parent)
    }
}

/// The boot-shipped programs `SYS_SPAWN` may create a process from,
/// keyed by the same `module_string` cmdline Limine tags each one with
/// in `limine.conf` — everything `MODULES_REQUEST` handed the kernel
/// except the module named `"init"`, which boot code loads directly
/// rather than through this spawn-by-name path.
///
/// Held for the kernel's entire lifetime with no unsafe code beyond
/// what boot already does: `ModulesRequest::response()` returns
/// `Option<&'static Response<..>>`, so every `&File`/`&[u8]` obtained
/// from it is already `'static`, and it's safe to hold that memory
/// forever — Limine tags module regions `MEMMAP_EXECUTABLE_AND_MODULES`,
/// which `memory::phys::BitmapFrameAllocator::populate` never reclaims
/// (it only ever frees `MEMMAP_USABLE` regions).
static SPAWNABLE_MODULES: Once<Vec<(&'static str, &'static [u8])>> = Once::new();

/// Populates the spawn-by-name registry from Limine's modules response.
/// Must run once at boot, after the `MODULES_REQUEST` response is
/// available. There is no filesystem yet — this fixed, boot-time set is
/// the only source `SYS_SPAWN` has for a new process's code (see
/// `docs/adr/0006-dynamic-process-creation-and-capability-transfer.md`).
pub fn init_spawnable_modules(modules: &'static [&'static limine::file::File]) {
    SPAWNABLE_MODULES.call_once(|| {
        modules
            .iter()
            .filter(|m| m.cmdline() != "init")
            .map(|m| (m.cmdline(), m.data()))
            .collect()
    });
}

/// Looks up a boot-shipped program's ELF bytes by its `module_string`
/// name, for `SYS_SPAWN`. `None` if no such module was shipped (or
/// [`init_spawnable_modules`] hasn't run yet, which should never happen
/// once boot completes).
pub fn lookup_spawnable_module(name: &str) -> Option<&'static [u8]> {
    SPAWNABLE_MODULES
        .get()?
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, data)| *data)
}
