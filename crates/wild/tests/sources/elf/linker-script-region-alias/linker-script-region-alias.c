//#AbstractConfig:default
//#RunEnabled:false
//#LinkArgs:-shared
//#ReferenceLinkers:bfd,lld
//#DiffIgnore:.dynamic.DT_FLAGS_1.NOW

//#Config:alias:default
//#LinkerScript:linker-script-region-alias.ld
//#ExpectSym:var1 address=0x10000000
//#ExpectSym:var3 address=0x10000004

static int var1 __attribute__((used, section(".data.ram1"))) = 0x01;
static int var3 __attribute__((used, section(".data.ram2"))) = 0x03;

void _start(void) {}
