PHDRS {
    text PT_LOAD FILEHDR PHDRS AT(0x1000);
    data PT_LOAD;
}

SECTIONS {
    . = SIZEOF_HEADERS;
    .text : {
        *(.text .text.*)
    } :text
    .data : {
        *(.data .data.*)
    } :data
}
