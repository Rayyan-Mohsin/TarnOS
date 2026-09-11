pub mod executor;
pub mod process;
pub mod scheduler;

/// A process identifier. Defined here (rather than in `task::process`,
/// which doesn't exist yet) because `ipc::endpoint::Waiter` needs to name
/// a process without depending on the process/scheduler machinery a
/// later milestone task adds.
///
/// Packs a process-table index (low 32 bits) and a generation counter
/// (high 32 bits) into the same `u64` — deliberately not two separate
/// fields, so every existing register-packing call site (`sys_grant`,
/// `sys_process_start`, and now `sys_wait`/`sys_kill`) needs no change
/// at all: a `Pid` is still exactly one `u64` register, in and out.
/// `MAX_PROCESSES` (16) leaves 32 bits of index headroom deliberately
/// generous; 32 bits of generation is enormous relative to any
/// realistic process-creation rate. The generation half exists so a
/// stale reference to a process that has since been torn down (e.g. a
/// `Waiter::Process(pid)` still queued in an `ipc::Endpoint`, or a
/// `Suspended` child's `parent` field) can never silently alias an
/// unrelated process that later reuses the same table slot — see
/// `task::scheduler::allocate_pid`/`with_process` and
/// `docs/adr/0007-process-lifecycle-and-termination.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pid(pub u64);

impl Pid {
    pub const fn new(index: usize, generation: u32) -> Self {
        Pid(index as u64 | ((generation as u64) << 32))
    }

    pub const fn index(self) -> usize {
        (self.0 & 0xFFFF_FFFF) as usize
    }

    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}
