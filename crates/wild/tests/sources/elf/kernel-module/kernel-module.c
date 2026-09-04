//#AbstractConfig:default
//#RunEnabled:false
//#LinkArgs:-r -T tests/sources/elf/kernel-module/kernel-module.ld
//#ReferenceLinkers:bfd
//#ExpectSym:mod_fn

//#Config:x86_64:default
//#Arch:x86_64

//#Config:aarch64:default
//#Arch:aarch64

//#Config:riscv64:default
//#Arch:riscv64

//#Config:loongarch64:default
//#Arch:loongarch64

//#Config:ppc64le:default
//#Arch:ppc64le

int mod_fn(void) { return 1; }
