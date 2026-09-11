STARTUP(startup-first.c.o)
ENTRY(_start)
SECTIONS {
    .order : {
        KEEP(*(.order))
    }
    .text : {
        *(.text .text.*)
    }
}
