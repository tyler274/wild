EXTERN(bar)
ENTRY(_start)
SECTIONS {
    .text : { *(.text .text.*) }
}
