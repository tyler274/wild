// GNU `INSERT AFTER` / `INSERT BEFORE` splice this script's SECTIONS into the
// default layout at the named output section, rather than replacing the script.
//
//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#DiffEnabled:false
//#SkipOverlapSegmentsCheck:true
//#LinkArgs:--no-gc-sections

//#Config:after:default
//#LinkerScript:linker-script-insert-after.ld
//#ExpectSection:.inserted after=".text"

//#Config:before:default
//#LinkerScript:linker-script-insert-before.ld
//#ExpectSection:.data after=".inserted_before"

__attribute__((section(".inserted"), used))
const unsigned char inserted = 1;

__attribute__((section(".inserted_before"), used))
unsigned char inserted_before = 2;

__attribute__((section(".data"), used))
unsigned char keep_data = 3;

void _start(void) {}
