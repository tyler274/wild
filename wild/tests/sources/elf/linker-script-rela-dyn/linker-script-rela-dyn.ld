ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    } :text

    .data : {
        KEEP(*(.data .data.*))
    } :data

    .rela.dyn : {
        *(.rela.*) *(.rela_*)
    }

    .symtab 0 : { *(.symtab) }
    .strtab 0 : { *(.strtab) }
    .shstrtab 0 : { *(.shstrtab) }

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .bss .bss.*)
    }
}

ASSERT(SIZEOF(.rela.dyn) == 0, "input reloc sections must not fill .rela.dyn");
