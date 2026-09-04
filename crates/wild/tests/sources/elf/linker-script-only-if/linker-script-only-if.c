// GNU `ONLY_IF_RO` / `ONLY_IF_RW`: the same output name appears twice. If all
// matching inputs are read-only, the first copy is used; if any is writable,
// every matching input goes to the second copy.
//
//#AbstractConfig:default
//#Arch: x86_64
//#Mode:dynamic
//#LinkArgs:-shared -z now
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-only-if.ld
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RW.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:ro:default
//#Object:only-if-ro.s
//#ExpectSection:.onlyif flags=A
//#ExpectSection:.marker after=".onlyif"
//#ExpectProgramHeader:LOAD flags=RX,sections=[.text,.onlyif,.marker,*]

//#Config:rw:default
//#Object:only-if-rw.s
//#ExpectSection:.onlyif flags=WA
//#ExpectSection:.data after=".onlyif"
//#ExpectProgramHeader:LOAD flags=RW,sections=[.onlyif,.data,*]

//#Config:mixed:default
//#Object:only-if-ro.s
//#Object:only-if-rw.s
//#ExpectSection:.onlyif flags=WA
//#ExpectSection:.data after=".onlyif"
//#ExpectProgramHeader:LOAD flags=RW,sections=[.onlyif,.data,*]

__attribute__((section(".marker"))) const char marker = 3;
int data_item = 4;

void _start(void) {}
