// Glibc generates shlib.lds from GNU ld's `--verbose` default shared script.
// This test feeds that script to Wild and checks that a PIC .so still links.
//
//#AbstractConfig:default
//#Arch:x86_64
//#Mode:dynamic
//#CompArgs:-fPIC
//#LinkerScript:gnu-default-shared.ld
//#LinkArgs:-shared -z now
//#RunEnabled:false
// Layout may still differ from GNU in other ways; linking is the glibc gate.
//#DiffEnabled:false
//#ReferenceLinkers:bfd
//#ExpectDynSym:foo
//#ExpectDynSym:data_item
// Script `.comment` is the same section as Wild's identity.
//#ExpectComment:GCC*

//#Config:shared:default

int data_item = 42;

int foo(void) {
  return data_item;
}
