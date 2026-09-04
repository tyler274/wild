PHDRS {
    text PT_LOAD FILEHDR PHDRS FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = SIZEOF_HEADERS;
    .text : { *(.text*) } :text
    .onlyif : ONLY_IF_RO { KEEP(*(.onlyif)) } :text
    .marker : { KEEP(*(.marker)) } :text
    .onlyif : ONLY_IF_RW { KEEP(*(.onlyif)) } :data
    .data : { KEEP(*(.data*)) } :data
}
