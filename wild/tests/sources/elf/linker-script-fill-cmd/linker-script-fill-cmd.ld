SECTIONS {
    .fill1 : {
        FILL(0x90909090)
        *(.fill1.first)
        *(.fill1.second)
    }
}
