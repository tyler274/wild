//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-comment.ld
// Script `.comment` shares Wild's identity section; both strings are in one
// `.comment` (GNU ld's LINKER_VERSION is a nop without --enable-linker-version).
//#DiffEnabled:false
//#ExpectComment:GCC*

//#Config:basic:default

void _start(void) {}
