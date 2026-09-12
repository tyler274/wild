ENTRY(_start)

SECTIONS {
    . = 0x401000;
    .text : { *(.text) }
    .ro.flags : { KEEP(INPUT_SECTION_FLAGS(SHF_ALLOC & !SHF_WRITE) *(.sec.flags)) }
    .rw.flags : { KEEP(INPUT_SECTION_FLAGS(SHF_WRITE) *(.sec.flags)) }
    .eh_frame : { *(.eh_frame) }
}
