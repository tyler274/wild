ENTRY(_start)

SECTIONS {
    . = 0x400000;
    .text : {
        startup_64 = .;
        _stext = .;
        KEEP(*(.text .text.*))
        _etext = .;
    }
    phys_startup_64 = ABSOLUTE(startup_64 - 0x400000);
    text_size = _etext - _stext;
    .data : {
        KEEP(*(.data .data.*))
    }
    .bss : {
        KEEP(*(.bss .bss.*))
        *(COMMON)
    }
    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .rodata*)
    }
}

jiffies = jiffies_64;
jiffies_plus = jiffies_64 + 4;
const_current_task = current_task;
__ref_stack_chk_guard = __stack_chk_guard;
