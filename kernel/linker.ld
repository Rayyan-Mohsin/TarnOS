/* TarnOS kernel linker script.
 *
 * Places the kernel at Limine's default higher-half base and keeps every
 * loadable segment single-permission (never both writable and executable),
 * which is a cheap, link-time-enforced form of W^X.
 */
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)

ENTRY(_start)

PHDRS
{
    text    PT_LOAD FLAGS(0x5); /* Read + Execute */
    rodata  PT_LOAD FLAGS(0x4); /* Read only */
    data    PT_LOAD FLAGS(0x6); /* Read + Write */
}

SECTIONS
{
    . = 0xffffffff80000000;

    __kernel_start = .;

    .text : {
        *(.text .text.*)
    } :text
    . = ALIGN(4096);

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata
    . = ALIGN(4096);

    .data : {
        /* Limine requests: must be kept even though nothing else references them. */
        KEEP(*(.requests_start))
        KEEP(*(.requests))
        KEEP(*(.requests_end))
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data
    . = ALIGN(4096);

    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
