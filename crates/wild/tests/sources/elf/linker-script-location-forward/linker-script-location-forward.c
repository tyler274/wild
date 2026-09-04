//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#SkipArch:riscv64,ppc64le
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:chain:default
//#LinkerScript:linker-script-location-forward.ld
//#ExpectSym:_stext address=0x401000,section=".text"
//#ExpectSym:later_sum address=0x401000,section="ABS"

//#Config:const:default
//#LinkerScript:linker-script-location-forward-const.ld
//#ExpectSym:_stext address=0x500000,section=".text"
//#ExpectSym:later_const address=0x500000,section="ABS"

//#Config:provide:default
//#LinkerScript:linker-script-location-forward-provide.ld
//#ExpectSym:_stext address=0x500000,section=".text"

void _start(void) {}
int payload = 1;
