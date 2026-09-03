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
// Layout still differs from GNU (ONLY_IF_RO keeps the first .eh_frame copy;
// ALIGNOF(NEXT_SECTION) is a no-op). Linking is the glibc gate.
//#DiffEnabled:false
//#ReferenceLinkers:bfd
//#ExpectDynSym:foo
//#ExpectDynSym:data_item
// The script's `.comment` is a separate INFO section from Wild's identity
// `.comment`; section_by_name sees GCC's string first.
//#ExpectComment:GCC*

//#Config:shared:default

int data_item = 42;

int foo(void) {
  return data_item;
}
