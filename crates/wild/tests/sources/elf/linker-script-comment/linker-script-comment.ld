ENTRY(_start)

SECTIONS {
    .text : { KEEP(*(.text*)) }
    .comment 0 (INFO) : { *(.comment); LINKER_VERSION; }
    /DISCARD/ : { *(.note .note.* .eh_frame .eh_frame_hdr) }
}
