HIDDEN(hidden_abs = 0x1234);
visible_abs = 0x5678;

SECTIONS {
    .text : {
        HIDDEN(_hidden_start = .);
        *(.text .text.*)
        visible_end = .;
    }
}
