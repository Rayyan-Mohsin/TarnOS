//! Minimal ELF64 loader for static, non-PIE, `ET_EXEC` executables — no
//! dynamic linking or relocation, matching exactly what `tarnos-rt`-based
//! userland binaries are built as.
//!
//! Never trusts the input blindly: every `PT_LOAD` segment's declared
//! address range and file offsets are validated against a permitted user
//! range and the actual input length before anything is mapped, the same
//! scrutiny a less-trusted future binary would need to go through. Maps
//! through the caller-supplied [`Mapper`], so this works unchanged
//! against either the boot-time kernel mapper or a fresh per-process
//! address space once one exists.
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

use crate::memory::virt::phys_to_virt;

const EI_MAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

/// Lowest permitted user virtual address — excludes the null-page guard
/// range so a null-pointer dereference in userland reliably faults.
const USER_SPACE_MIN: u64 = 0x1000;
/// Highest permitted user virtual address: just under the top of the
/// canonical lower half, leaving the rest of that half for a future user
/// stack/mmap region and keeping well clear of the canonical upper half
/// the kernel and HHDM occupy.
const USER_SPACE_MAX: u64 = 0x0000_7fff_ffff_f000;

#[derive(Debug)]
pub enum ElfError {
    TooShort,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    NotExecutable,
    WrongMachine,
    SegmentOutOfRange,
    SegmentExceedsFile,
    OutOfMemory,
    MapFailed,
}

pub struct LoadedElf {
    pub entry: VirtAddr,
}

struct ProgramHeader {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_filesz: u64,
    p_memsz: u64,
}

fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(data[off..off + 2].try_into().unwrap())
}
fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}
fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

/// Loads a static ELF64 `ET_EXEC` image, mapping its `PT_LOAD` segments
/// through `mapper` (backed by frames from `frame_allocator`) and
/// returning its entry point. Zeroes every page before copying file
/// bytes in, so a segment's `p_memsz > p_filesz` tail (BSS) comes out
/// correctly zeroed with no extra bookkeeping.
pub fn load(
    data: &[u8],
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<LoadedElf, ElfError> {
    if data.len() < EHDR_SIZE {
        return Err(ElfError::TooShort);
    }
    if data[0..4] != EI_MAG {
        return Err(ElfError::BadMagic);
    }
    if data[4] != ELFCLASS64 {
        return Err(ElfError::NotElf64);
    }
    if data[5] != ELFDATA2LSB {
        return Err(ElfError::NotLittleEndian);
    }

    let e_type = read_u16(data, 16);
    let e_machine = read_u16(data, 18);
    let e_entry = read_u64(data, 24);
    let e_phoff = read_u64(data, 32) as usize;
    let e_phentsize = read_u16(data, 54) as usize;
    let e_phnum = read_u16(data, 56) as usize;

    if e_type != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if e_machine != EM_X86_64 {
        return Err(ElfError::WrongMachine);
    }
    if e_phentsize < PHDR_SIZE {
        return Err(ElfError::TooShort);
    }

    let phdrs_end = e_phoff
        .checked_add(e_phnum.checked_mul(e_phentsize).ok_or(ElfError::TooShort)?)
        .ok_or(ElfError::TooShort)?;
    if phdrs_end > data.len() {
        return Err(ElfError::TooShort);
    }

    for i in 0..e_phnum {
        let base = e_phoff + i * e_phentsize;
        let phdr = ProgramHeader {
            p_type: read_u32(data, base),
            p_flags: read_u32(data, base + 4),
            p_offset: read_u64(data, base + 8),
            p_vaddr: read_u64(data, base + 16),
            p_filesz: read_u64(data, base + 32),
            p_memsz: read_u64(data, base + 40),
        };

        if phdr.p_type != PT_LOAD || phdr.p_memsz == 0 {
            continue;
        }

        load_segment(&phdr, data, mapper, frame_allocator)?;
    }

    Ok(LoadedElf {
        entry: VirtAddr::new(e_entry),
    })
}

fn load_segment(
    phdr: &ProgramHeader,
    data: &[u8],
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), ElfError> {
    let seg_end = phdr
        .p_vaddr
        .checked_add(phdr.p_memsz)
        .ok_or(ElfError::SegmentOutOfRange)?;
    if phdr.p_vaddr < USER_SPACE_MIN || seg_end > USER_SPACE_MAX {
        return Err(ElfError::SegmentOutOfRange);
    }
    if phdr.p_filesz > phdr.p_memsz {
        return Err(ElfError::SegmentOutOfRange);
    }
    let file_end = phdr
        .p_offset
        .checked_add(phdr.p_filesz)
        .ok_or(ElfError::SegmentExceedsFile)?;
    if file_end > data.len() as u64 {
        return Err(ElfError::SegmentExceedsFile);
    }

    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if phdr.p_flags & PF_W != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if phdr.p_flags & PF_X == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }

    let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(phdr.p_vaddr));
    let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(seg_end - 1));
    for page in Page::range_inclusive(start_page, end_page) {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(ElfError::OutOfMemory)?;
        unsafe {
            core::ptr::write_bytes(phys_to_virt(frame.start_address()).as_mut_ptr::<u8>(), 0, 4096);
        }
        unsafe { mapper.map_to(page, frame, flags, frame_allocator) }
            .map_err(|_| ElfError::MapFailed)?
            .flush();
    }

    let mut remaining = &data[phdr.p_offset as usize..file_end as usize];
    let mut vaddr = VirtAddr::new(phdr.p_vaddr);
    while !remaining.is_empty() {
        let page = Page::<Size4KiB>::containing_address(vaddr);
        let page_offset = (vaddr.as_u64() - page.start_address().as_u64()) as usize;
        let chunk_len = core::cmp::min(remaining.len(), 4096 - page_offset);
        let frame = mapper
            .translate_page(page)
            .expect("page was just mapped by the loop above");
        let dst = phys_to_virt(frame.start_address() + page_offset as u64);
        unsafe {
            core::ptr::copy_nonoverlapping(remaining.as_ptr(), dst.as_mut_ptr::<u8>(), chunk_len);
        }
        remaining = &remaining[chunk_len..];
        vaddr += chunk_len as u64;
    }

    Ok(())
}
