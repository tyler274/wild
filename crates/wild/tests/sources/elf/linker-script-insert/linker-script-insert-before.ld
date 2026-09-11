SECTIONS {
    .inserted_before : {
        KEEP(*(.inserted_before))
    }
}
INSERT BEFORE .data;
