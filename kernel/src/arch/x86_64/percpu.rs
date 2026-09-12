//! Per-core state: a fixed table of slots, one per possible CPU core,
//! looked up by a cheap `CPUID` read of the calling core's own local APIC
//! ID rather than a dedicated per-core CPU register (`GS_BASE`/`%gs`).
//!
//! The simpler, indexed-lookup mechanism was chosen deliberately over true
//! GS-relative per-core storage: it needs no new MSR/register-setup
//! surface at bring-up, at the cost of a linear scan over
//! [`MAX_CORES`] slots on each lookup — cheap at this milestone's scale,
//! and a GS-relative mechanism can be retrofitted later without changing
//! any caller if a hot path ever needs it.
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Fixed compile-time ceiling on the number of CPU cores this kernel will
/// ever bring up — mirrors `task::scheduler::MAX_PROCESSES`'s own
/// "generous fixed ceiling, log-don't-panic on overflow" convention. If
/// Limine reports more cores than this, only the first `MAX_CORES` are
/// started (see `arch::x86_64::smp::bring_up_aps`).
pub const MAX_CORES: usize = 8;

/// Sentinel `lapic_id` value for a slot no core has been assigned to yet.
/// A real xAPIC ID never exceeds `0xFF`, so this can never collide.
const UNASSIGNED: u32 = u32::MAX;

/// One core's own state. Every field is atomic because the whole
/// [`SLOTS`] table is a single `static` — there is no per-instance
/// exclusive ownership the way there is for e.g. `task::process::Process`.
pub struct PerCpuSlot {
    lapic_id: AtomicU32,
    /// Set by the core itself once it has finished its own bring-up
    /// sequence (GDT/IDT/LAPIC) and is about to enter its idle loop.
    pub ready: AtomicBool,
    /// Bumped by this core's test-IPI handler — see `arch::x86_64::lapic`
    /// and `xtask test-smp-ipi`.
    pub ipi_count: AtomicU64,
    /// Bumped in a free-running loop only under the `smp-boot-test`
    /// feature, as evidence this core is genuinely executing
    /// concurrently with every other one rather than being secretly
    /// serialized — see `xtask test-smp-boot`.
    pub spin_count: AtomicU64,
}

impl PerCpuSlot {
    const fn new() -> Self {
        Self {
            lapic_id: AtomicU32::new(UNASSIGNED),
            ready: AtomicBool::new(false),
            ipi_count: AtomicU64::new(0),
            spin_count: AtomicU64::new(0),
        }
    }

    pub fn lapic_id(&self) -> u32 {
        self.lapic_id.load(Ordering::Acquire)
    }
}

static SLOTS: [PerCpuSlot; MAX_CORES] = [const { PerCpuSlot::new() }; MAX_CORES];

/// Assigns `lapic_id` to slot `index`, making it discoverable by
/// [`core_index`] from that point on. Called only by
/// `smp::bring_up_aps` on the BSP, once per core, strictly before that
/// core is ever started (index 0, the BSP's own slot, before
/// `arch::x86_64::init` completes; every AP's slot before its
/// `MpInfo::bootstrap()` call) — so every reader either hasn't started
/// yet or observes this write already complete via the
/// release/acquire pair `MpInfo::bootstrap`/`extra_argument` already
/// establishes.
pub fn assign_slot(index: usize, lapic_id: u32) {
    SLOTS[index].lapic_id.store(lapic_id, Ordering::Release);
}

/// The slot for a given compacted core index (`0` = BSP, `1..MAX_CORES`
/// = APs in the order `smp::bring_up_aps` assigned them).
pub fn slot(index: usize) -> &'static PerCpuSlot {
    &SLOTS[index]
}

/// This core's own local APIC ID, read fresh via `CPUID` — exposed for
/// `smp::bring_up_aps`'s fallback path when Limine didn't honor
/// `MP_REQUEST` at all (so there is no `MpRespData::bsp_lapic_id` to
/// register the BSP's own slot with instead).
pub fn read_own_apic_id() -> u32 {
    read_initial_apic_id()
}

/// This core's own compacted index, found by reading its local APIC ID
/// via `CPUID` (leaf 1, EBX bits 31:24 — the "initial APIC ID," valid on
/// every x86_64 CPU regardless of xAPIC/x2APIC mode) and scanning
/// [`SLOTS`] for a match.
///
/// # Panics
/// If this core's LAPIC ID was never assigned a slot via
/// [`assign_slot`] — meaning it started running before its own bring-up
/// wrote that slot, which should never happen given the ordering
/// [`assign_slot`]'s doc comment describes.
pub fn core_index() -> usize {
    let apic_id = read_initial_apic_id();
    SLOTS
        .iter()
        .position(|slot| slot.lapic_id() == apic_id)
        .expect("this core's LAPIC ID has no assigned percpu slot")
}

/// Reads `CPUID` leaf 1 and returns EBX bits 31:24 (the initial local
/// APIC ID). `ebx`/`rbx` cannot be used as an `asm!` output/clobber
/// register directly (LLVM reserves it), hence the save-to-a-temporary
/// dance below — the standard workaround for `cpuid` in Rust inline asm.
fn read_initial_apic_id() -> u32 {
    let ebx: u32;
    unsafe {
        core::arch::asm!(
            "mov {tmp:e}, ebx",
            "cpuid",
            "mov {out:e}, ebx",
            "mov ebx, {tmp:e}",
            inout("eax") 1u32 => _,
            tmp = out(reg) _,
            out = out(reg) ebx,
            out("ecx") _,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
    ebx >> 24
}
