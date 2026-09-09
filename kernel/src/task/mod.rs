pub mod executor;

/// A process identifier. Defined here (rather than in `task::process`,
/// which doesn't exist yet) because `ipc::endpoint::Waiter` needs to name
/// a process without depending on the process/scheduler machinery a
/// later milestone task adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pid(pub u64);
