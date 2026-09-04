//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkArgs:--no-gc-sections
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:ignore-input-relocs:default
//#LinkerScript:linker-script-rela-dyn.ld
//#NoSection:.rela.data

void _start(void) {}
int payload = 1;
int *addr = &payload;
