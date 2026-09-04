/* GNU ld default shared-object DATA_SEGMENT_* / CONSTANT builtins (glibc shlib.lds). */
SECTIONS {
  . = SEGMENT_START("text-segment", 0) + SIZEOF_HEADERS;
  .text : { *(.text .text.*) }
  .rodata : { *(.rodata .rodata.*) }
  . = DATA_SEGMENT_ALIGN (CONSTANT (MAXPAGESIZE), CONSTANT (COMMONPAGESIZE));
  data_seg = .;
  .data : { *(.data .data.*) }
  . = DATA_SEGMENT_RELRO_END (0, .);
  .bss : { *(.bss .bss.*) }
  . = DATA_SEGMENT_END (.);
}
