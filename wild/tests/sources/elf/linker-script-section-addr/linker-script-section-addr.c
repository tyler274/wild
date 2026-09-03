//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-section-addr.ld
//#SkipOverlapSegmentsCheck:true
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment
//#ExpectSym:_stext address=0x400004,section=".text"
//#ExpectSym:data_start section=".data"
//#ExpectSym:bss_start section=".bss"

//#Config:basic:default

void _start(void) {}
int payload = 1;
char bss_byte __attribute__((used));
