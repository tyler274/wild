//#AbstractConfig:default
//#RunEnabled:false
//#LinkArgs:-T tests/sources/elf/kernel-script/kernel-like.ld
//#ReferenceLinkers:bfd
//#SkipOverlapSegmentsCheck:true
//#ExpectSym:_stext
//#ExpectSym:__init_begin
//#ExpectSym:__global_pointer$

//#Config:x86_64:default
//#Arch:x86_64
//#SkipOverlapSegmentsCheck:true

//#Config:aarch64:default
//#Arch:aarch64

//#Config:riscv64:default
//#Arch:riscv64

//#Config:loongarch64:default
//#Arch:loongarch64

//#Config:ppc64le:default
//#Arch:ppc64le

int data_var = 1;

void _start(void) {
    (void)data_var;
}
