// Same-size swap of two ints so `target` moves without changing input section sizes.
// The unchanged object (incremental_reloc_user.s) has a direct PC32 load of `target`.
// Without reverse-reloc patching, that site still points at the old address.

//#Object:incremental_reloc_user.s
//#Object:runtime.c
//#LinkArgs:-nostdlib -znow
//#WildExtraLinkArgs:--incremental
//#TestIncremental:true
//#IncrementalExpect:42
//#DiffEnabled:false
//#Arch: x86_64

#ifdef WILD_INC
static int pad __attribute__((used, section(".data"))) = 1;
int target __attribute__((section(".data"))) = 42;
#else
int target __attribute__((section(".data"))) = 42;
static int pad __attribute__((used, section(".data"))) = 1;
#endif
