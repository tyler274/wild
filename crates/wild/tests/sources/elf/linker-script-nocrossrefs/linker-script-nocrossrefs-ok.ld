SECTIONS {
    .a_sec : { KEEP(*(.a_sec)) }
    .b_sec : { KEEP(*(.b_sec)) }
    .text : { *(.text .text.*) }
}
