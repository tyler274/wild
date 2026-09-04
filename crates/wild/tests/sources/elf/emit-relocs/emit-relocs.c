//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkArgs:--emit-relocs --no-gc-sections
//#ExpectSection:.rela.text after=".text"
//#ExpectSection:.rela.data after=".data"
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default

//#Config:script:default
//#LinkerScript:emit-relocs.ld
//#NoSection:.rela.discard

int payload = 1;
int *addr = &payload;

int __attribute__((section(".discard"))) discarded = 1;
int *__attribute__((section(".discard"))) discard_ptr = &discarded;

__attribute__((section(".text.hot")))
int hot(void) {
    return payload;
}

void _start(void) {
    (void)addr;
    (void)hot();
}
