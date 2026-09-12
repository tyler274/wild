LD_FEATURE("NOT_A_FEATURE");
SECTIONS {
    .text : { *(.text .text.*) }
}
