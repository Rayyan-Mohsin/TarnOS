//! Kernel-internal IPC payload representation.
use tarnos_abi::Message;

/// How a message's bytes are actually stored.
///
/// Only [`MessagePayload::Inline`] is constructed this milestone — every
/// message fits in `tarnos_abi::Message`'s fixed inline capacity, passed
/// entirely in registers with no user-pointer validation needed.
/// `OutOfLine` is the designed extension point for larger future
/// payloads (e.g. a POSIX-era `write(2)` buffer, referenced by a mapped
/// page instead of copied through registers) — it exists so that
/// extension doesn't require reshaping every call site that touches a
/// message today, but nothing constructs it yet.
pub enum MessagePayload {
    Inline(Message),
    #[allow(dead_code)]
    OutOfLine {
        page: crate::memory::PhysAddr,
        len: usize,
    },
}

impl MessagePayload {
    pub fn inline(message: Message) -> Self {
        MessagePayload::Inline(message)
    }

    /// Recovers the inline message. Panics on `OutOfLine`, which nothing
    /// constructs yet — once something does, this becomes the point
    /// where an out-of-line payload gets copied in or mapped for the
    /// receiver instead.
    pub fn into_inline(self) -> Message {
        match self {
            MessagePayload::Inline(m) => m,
            MessagePayload::OutOfLine { .. } => {
                unreachable!("out-of-line messages are not constructed this milestone")
            }
        }
    }
}
