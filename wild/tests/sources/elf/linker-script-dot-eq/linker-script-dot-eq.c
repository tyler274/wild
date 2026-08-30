//#AbstractConfig:default
//#Config:basic:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#SkipOverlapSegmentsCheck:true
//#LinkerScript:linker-script-dot-eq.ld
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment
//#ExpectSym:alias_sym address=0x1000
//#ExpectSym:payload_sym address=0x1100
//#ExpectSym:after_stack address=0x2400

__attribute__((section(".text.anchor"), used, noinline, noclone))
int alias_sym(void) {
    return 1;
}

__attribute__((section(".text.payload"), used, noinline, noclone))
int payload_sym(void) {
    return 2;
}

__attribute__((section(".data.stack"), used))
char stack_pad = 1;

__attribute__((section(".data.after"), used))
char after_stack = 2;

void _start(void) {
    (void)alias_sym();
    (void)payload_sym();
    (void)stack_pad;
    (void)after_stack;
}
