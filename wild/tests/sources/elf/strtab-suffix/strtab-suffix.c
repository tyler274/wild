//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#SkipArch:riscv64,ppc64le
//#LinkArgs:--no-gc-sections
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default
//#ExpectSym:strtab_suffix_share_host
//#ExpectSym:suffix_share_host

int strtab_suffix_share_host = 1;
int suffix_share_host = 2;

void _start(void) {
    (void)strtab_suffix_share_host;
    (void)suffix_share_host;
}
