// GNU OUTPUT(filename) is the same as `-o`. The test harness always passes `-o`,
// which wins, so this only checks that OUTPUT is accepted.
//
//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#Arch: x86_64
//#DiffEnabled:false

//#Config:output:default
//#LinkerScript:linker-script-output.ld

void _start(void) {}
