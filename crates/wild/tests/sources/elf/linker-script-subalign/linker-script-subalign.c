// GNU `SUBALIGN(n)` forces every input in the output section to alignment `n`,
// whether that is larger or smaller than `sh_addralign`. Output alignment is
// `max(ALIGN, SUBALIGN)` and is not raised by input `sh_addralign`.
//
//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#Object:linker-script-subalign-a.c
//#Object:linker-script-subalign-b.c
//#SkipOverlapSegmentsCheck:true
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:subalign:default
//#LinkerScript:linker-script-subalign.ld
//#ExpectSym:small_s address=0x400000
//#ExpectSym:big_s address=0x400004
//#ExpectSectionBytes:.s=0x0100000002 0..5

//#Config:align-and-subalign:default
//#LinkerScript:linker-script-subalign-align.ld
//#ExpectSym:small_s address=0x400000
//#ExpectSym:big_s address=0x400004
//#ExpectSectionBytes:.s=0x0100000002 0..5

//#Config:larger:default
//#LinkerScript:linker-script-subalign-larger.ld
//#ExpectSym:small_s address=0x400000
//#ExpectSym:big_s address=0x400020

void _start(void) {}
