// `.suffix` is a suffix of `.host.suffix`, so GNU ld stores it inside `.shstrtab`.

//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#SkipArch:riscv64,ppc64le
//#LinkArgs:--no-gc-sections
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default
//#ExpectSection:.suffix
//#ExpectSection:.host.suffix

int suffix_sym __attribute__((used, section(".suffix"))) = 1;
int host_suffix_sym __attribute__((used, section(".host.suffix"))) = 2;

void _start(void) {
    (void)suffix_sym;
    (void)host_suffix_sym;
}
