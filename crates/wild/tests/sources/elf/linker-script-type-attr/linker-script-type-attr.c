//#Config:default
//#RunEnabled:false
//#LinkArgs:-shared
//#ReferenceLinkers:bfd
//#DiffIgnore:.dynamic.DT_FLAGS_1.NOW
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RWX.alignment
//#DiffIgnore:segment.LOAD.RX.alignment
//#LinkerScript:linker-script-type-attr.ld
//#ExpectSection:.note_type flags=A,type=0x7
//#ExpectSection:.init_type flags=A,type=0xe
//#ExpectSection:.numeric_type flags=A,type=0x7
//#ExpectSection:.expr_type flags=A,type=0x7
//#ExpectSection:.ro_note flags=A,type=0x7
//#ExpectSection:.input_keeps flags=WA,type=0x1

__attribute__((section(".section.keep"))) char keep = 0;
