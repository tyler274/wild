//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:lld
//#Arch: x86_64
//#DiffEnabled:false

//#Config:target:default
//#LinkerScript:linker-script-target.ld

//#Config:unsupported:default
//#ReferenceLinkers:
//#LinkerScript:linker-script-target-unsupported.ld
//#ExpectError:elf32-i386 is not yet supported

//#Config:mismatch:default
//#ReferenceLinkers:
//#LinkerScript:linker-script-target-mismatch.ld
//#ExpectError:Setting the input format using TARGET is currently unsupported

void _start() {}
