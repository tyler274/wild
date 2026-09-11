SECTIONS {
    .inserted : {
        KEEP(*(.inserted))
    }
}
INSERT AFTER .text;
