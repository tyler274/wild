//#AbstractConfig:default
//#RunEnabled:false
//#LinkArgs:--no-gc-sections
//#Arch: x86_64
//#DiffEnabled:false

//#Config:ok:default
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-nocrossrefs-ok.ld

//#Config:fail:default
//#ReferenceLinkers:
//#LinkerScript:linker-script-nocrossrefs.ld
//#ExpectError:prohibited cross reference

extern char b_sym;
__attribute__((section(".a_sec"), used)) char *a_ptr = &b_sym;
__attribute__((section(".b_sec"), used)) char b_sym = 1;

void _start(void) {}
