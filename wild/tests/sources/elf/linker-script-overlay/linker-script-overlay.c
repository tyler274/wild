//#AbstractConfig:default
//#Config:basic:default
//#RunEnabled:false
//#LinkArgs:-T tests/sources/elf/linker-script-overlay/linker-script-overlay.ld
//#ReferenceLinkers:bfd
//#SkipOverlapSegmentsCheck:true
//#ExpectSym:__load_start_ovl1
//#ExpectSym:__load_stop_ovl1
//#ExpectSym:__load_start_ovl2

static char ovl1 __attribute__((used, section(".ovl1"))) = 1;
static char ovl2 __attribute__((used, section(".ovl2"))) = 2;

extern char __load_start_ovl1;
extern char __load_stop_ovl1;
extern char __load_start_ovl2;
void *keep_overlay_syms[] __attribute__((used)) = {
    &__load_start_ovl1, &__load_stop_ovl1, &__load_start_ovl2};

void _start(void) {}
