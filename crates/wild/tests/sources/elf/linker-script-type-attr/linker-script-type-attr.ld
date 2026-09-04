SECTIONS {
    .note_type (TYPE = SHT_NOTE) : { BYTE(1) }
    .init_type (TYPE = SHT_INIT_ARRAY) : { BYTE(1) }
    .numeric_type (TYPE = 7) : { BYTE(1) }
    .expr_type (TYPE = 3 + 4) : { BYTE(1) }
    .ro_note (READONLY (TYPE = SHT_NOTE)) : { BYTE(1) }
    .input_keeps (TYPE = SHT_NOTE) : { *(.section.keep) }

    /DISCARD/ : { *(*) }
}
