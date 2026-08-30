SECTIONS {
    .text : {
        *(.text .text.*)
        included_sym = .;
    }
}
