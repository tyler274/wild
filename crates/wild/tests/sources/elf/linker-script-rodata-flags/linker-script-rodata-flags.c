//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkArgs:-shared -z now
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:mixed:default
//#LinkerScript:linker-script-rodata-flags.ld
//#ExpectSection:.rodata flags=A

const int keep = 42;
const char *msg = "hello";

void _start(void) {}
