pub mod capability;
pub mod endpoint;
pub mod message;

pub use capability::{CapTable, CapabilitySlot, KernelObjectRef, Rights};
pub use endpoint::Endpoint;
