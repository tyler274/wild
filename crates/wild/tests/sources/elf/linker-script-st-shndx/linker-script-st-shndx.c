//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-st-shndx.ld
//#LinkArgs:--no-gc-sections
//#SkipArch:riscv64,ppc64le
//#ExpectSym:jiffies section=".data",type=object
//#ExpectSym:jiffies_plus section="ABS"
//#ExpectSym:const_current_task section=".data",type=object
//#ExpectSym:startup_64 section=".text"
//#ExpectSym:phys_startup_64 section="ABS"
//#ExpectSym:text_size section="ABS"
//#ExpectSym:__ref_stack_chk_guard section=".bss",type=object
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default

// First matcher of `.data` so `jiffies_64` lands in a secondary (later KEEP).
unsigned long data_pad __attribute__((section(".data.pad"))) = 0;
unsigned long jiffies_64 __attribute__((section(".data"))) = 1;
int current_task __attribute__((section(".data"))) = 2;
unsigned long __stack_chk_guard;

extern unsigned long __ref_stack_chk_guard;
extern unsigned long jiffies;
extern unsigned long phys_startup_64;
unsigned long *keep_ref __attribute__((used, section(".data"))) = &__ref_stack_chk_guard;

void _start(void) {
    (void)data_pad;
    (void)jiffies_64;
    (void)current_task;
    (void)__stack_chk_guard;
    (void)keep_ref;
    (void)jiffies;
    (void)phys_startup_64;
}
