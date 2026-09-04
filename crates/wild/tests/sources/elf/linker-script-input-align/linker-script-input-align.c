//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#Object:linker-script-input-align-small.c
//#Object:linker-script-input-align-big.c
//#SkipOverlapSegmentsCheck:true
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:mixed:default
//#LinkerScript:linker-script-input-align.ld
//#ExpectSym:small_blob address=0x400000
//#ExpectSym:big_blob address=0x400100

void _start(void) {}
