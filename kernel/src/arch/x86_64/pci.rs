//! Minimal PCI configuration-space access: Configuration Mechanism #1
//! (`CONFIG_ADDRESS`/`CONFIG_DATA`, ports `0xCF8`/`0xCFC`) — the legacy,
//! unconditionally-present mechanism every PC-compatible chipset since
//! the original PCI spec supports, requiring no ACPI table lookup first
//! (unlike the newer, optional MMIO/ECAM mechanism). Confirmed against
//! this exact environment's QEMU (`docs/MILESTONE-11-BLOCK-STORAGE-DRIVER.md`'s
//! own Phase 1 findings) before writing this, not assumed from the spec
//! alone.
//!
//! Deliberately not a general-purpose PCI subsystem: [`find_device`]
//! brute-force-scans every bus/device/function far enough to find one
//! specific device by vendor/device ID, matching exactly what this
//! milestone's own Scope calls for — a registry other future drivers are
//! assumed to register against can be built once a second PCI device
//! actually needs one.
//!
//! `#[allow(dead_code)]` below is temporary, not a Phase 2/3-boundary
//! permanent justification the way Milestone 10's own
//! `percpu::spin_count`/`memory::virt::translate` ones are: this
//! milestone's own Phase 3/4 will make the virtio-blk driver call
//! `find_device` unconditionally, at which point this module is
//! permanently live and this attribute should come back out. Right now
//! its only caller is `main.rs`'s `block-driver-test`-gated boot block.
#![cfg_attr(not(feature = "block-driver-test"), allow(dead_code))]
use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Identifies one function of one PCI device — a bus/device/function
/// triple — and reads/writes its configuration space directly. Cheap to
/// copy; carries no cached state of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciAddress {
    bus: u8,
    device: u8,
    function: u8,
}

impl PciAddress {
    /// Builds the 32-bit value `CONFIG_ADDRESS` expects: enable bit set,
    /// this address's bus/device/function packed in, `offset` masked to
    /// its DWORD-aligned register number (PCI Local Bus spec §3.2.2.3.2)
    /// — the low two bits are always zero, since configuration space is
    /// only ever addressed a whole DWORD at a time; [`read_u16`]/
    /// [`read_u8`] extract their narrower result from the DWORD this
    /// returns.
    fn config_address(self, offset: u8) -> u32 {
        0x8000_0000
            | (self.bus as u32) << 16
            | (self.device as u32) << 11
            | (self.function as u32) << 8
            | (offset as u32 & 0xFC)
    }

    /// Reads one DWORD (4 bytes) of this function's configuration space
    /// at `offset` (rounded down to the nearest DWORD boundary).
    fn read_u32(self, offset: u8) -> u32 {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        // SAFETY: `CONFIG_ADDRESS`/`CONFIG_DATA` are the fixed,
        // architecturally-mandated ports every PC-compatible PCI host
        // bridge implements (confirmed against this environment's own
        // QEMU — see this module's doc comment); writing an address then
        // reading the paired data port back is the documented, side-
        // effect-free way to read configuration space (PCI Local Bus
        // spec §3.2.2.3).
        unsafe {
            addr_port.write(self.config_address(offset));
            data_port.read()
        }
    }

    /// Read-modify-writes one DWORD, replacing only the two bytes at
    /// `offset`'s 16-bit-aligned position — used by [`PciDevice::set_command`]
    /// to update the Command register (offset `0x04`, the low half of the
    /// same DWORD Status occupies the high half of) without disturbing
    /// Status's own read-to-clear bits by writing back whatever was last
    /// read there.
    fn write_u16(self, offset: u8, value: u16) {
        let dword_offset = offset & !0b11;
        let shift = (offset & 0b11) * 8;
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        let existing = self.read_u32(dword_offset);
        let mask = 0xFFFFu32 << shift;
        let merged = (existing & !mask) | ((value as u32) << shift);
        // SAFETY: same as `read_u32` — a documented, well-formed
        // configuration-space write to this exact function's own
        // registers.
        unsafe {
            addr_port.write(self.config_address(dword_offset));
            data_port.write(merged);
        }
    }

    fn read_u16(self, offset: u8) -> u16 {
        let dword = self.read_u32(offset & !0b11);
        let shift = (offset & 0b11) * 8;
        (dword >> shift) as u16
    }

    fn read_u8(self, offset: u8) -> u8 {
        let dword = self.read_u32(offset & !0b11);
        let shift = (offset & 0b11) * 8;
        (dword >> shift) as u8
    }

    /// `0xFFFF` means "no device present at this bus/device/function" —
    /// the PCI spec's own documented sentinel (a real vendor ID can never
    /// be all-ones).
    fn vendor_id(self) -> u16 {
        self.read_u16(0x00)
    }

    fn device_id(self) -> u16 {
        self.read_u16(0x02)
    }

    /// Bit 7 set means this device implements more than one function —
    /// only meaningful when read from function 0 (PCI Local Bus spec
    /// §6.2.1).
    fn header_type(self) -> u8 {
        self.read_u8(0x0E)
    }

    fn command(self) -> u16 {
        self.read_u16(0x04)
    }

    fn set_command(self, value: u16) {
        self.write_u16(0x04, value);
    }

    /// Reads BAR `index` (`0..6`, a type-0 header's own count) raw —
    /// undecoded: still tagged with its I/O-vs-memory/32-vs-64-bit/
    /// prefetchable bits in its own low bits, per PCI Local Bus spec
    /// §6.2.5.1. See [`PciDevice::mmio_bar_address`] for the decoded
    /// form this milestone's one real device actually needs.
    fn bar_raw(self, index: u8) -> u32 {
        self.read_u32(0x10 + index * 4)
    }
}

/// A PCI function [`find_device`] has already confirmed exists and
/// matches the caller's requested vendor/device ID — deliberately
/// minimal: just enough for a caller (a specific driver, e.g. this
/// milestone's virtio-blk one) to read its BARs and turn on decoding,
/// not a general device-model abstraction.
#[derive(Clone, Copy, Debug)]
pub struct PciDevice {
    pub address: PciAddress,
    pub vendor_id: u16,
    pub device_id: u16,
}

impl PciDevice {
    /// Resolves BAR `index`'s real, already-firmware-assigned base
    /// address, decoding the 32-bit-vs-64-bit memory-BAR encoding (PCI
    /// Local Bus spec §6.2.5.1) — never resizes or reassigns anything:
    /// QEMU's own firmware path already programs a working address into
    /// every BAR before the kernel ever gets control (confirmed directly
    /// via `info pci` — see this milestone's Phase 1 findings), the same
    /// "trust what's already there" precedent
    /// `arch::x86_64::lapic::init_mmio_mapping` already established for
    /// `IA32_APIC_BASE`. Panics if `index` names an I/O-space BAR —
    /// this milestone's one real device (a `-non-transitional` virtio-blk)
    /// has none.
    pub fn mmio_bar_address(self, index: u8) -> u64 {
        let raw = self.address.bar_raw(index);
        assert_eq!(
            raw & 0b1,
            0,
            "PCI device {:?} BAR {index} is an I/O-space BAR, not memory -- \
             this driver only ever expects a memory-space BAR",
            self.address
        );
        let is_64bit = (raw >> 1) & 0b11 == 0b10;
        let base_low = (raw & 0xFFFF_FFF0) as u64;
        if is_64bit {
            let high = self.address.bar_raw(index + 1);
            base_low | ((high as u64) << 32)
        } else {
            base_low
        }
    }

    /// Enables memory-space decoding (bit 1) and bus mastering (bit 2)
    /// in the Command register — necessary before touching any of this
    /// device's BAR-mapped MMIO regions (memory-space decode) or trusting
    /// it to perform DMA into kernel-supplied buffers (a virtqueue's own
    /// descriptor-ring reads, bus mastering). Firmware does not reliably
    /// enable either for a device an OS driver is about to claim — this
    /// must be done explicitly, not assumed already on.
    pub fn enable_mmio_and_bus_master(self) {
        const MEMORY_SPACE_ENABLE: u16 = 1 << 1;
        const BUS_MASTER_ENABLE: u16 = 1 << 2;
        let current = self.address.command();
        self.address
            .set_command(current | MEMORY_SPACE_ENABLE | BUS_MASTER_ENABLE);
    }
}

/// Scans every bus (`0..=255`), device (`0..32`), and — for a
/// multi-function device — function (`0..8`), returning the first one
/// whose vendor/device ID matches. Brute-force, not bridge-topology-aware
/// (no PCI-to-PCI bridge descent) — correct and simple for finding one
/// device on a flat bus-0 topology (confirmed via this milestone's own
/// Phase 1 findings: every device in this environment's `info pci`
/// output sits on bus 0), and still correct, just slower, on a deeper
/// real topology, since every bus is checked regardless. `None` if no
/// function anywhere matches.
pub fn find_device(vendor_id: u16, device_id: u16) -> Option<PciDevice> {
    for bus in 0..=u8::MAX {
        for device in 0..32u8 {
            let function_0 = PciAddress {
                bus,
                device,
                function: 0,
            };
            if function_0.vendor_id() == 0xFFFF {
                continue;
            }
            let function_count = if function_0.header_type() & 0x80 != 0 {
                8
            } else {
                1
            };
            for function in 0..function_count {
                let address = PciAddress {
                    bus,
                    device,
                    function,
                };
                let vid = address.vendor_id();
                if vid == 0xFFFF {
                    continue;
                }
                let did = address.device_id();
                if vid == vendor_id && did == device_id {
                    return Some(PciDevice {
                        address,
                        vendor_id: vid,
                        device_id: did,
                    });
                }
            }
        }
    }
    None
}
