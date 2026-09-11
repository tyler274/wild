// SEARCH_DIR is GNU `-L`. Combined with sysroot-relative `=/...`, INPUT(-l)
// finds an archive that was not passed on the command line.
//
//#Config:search-dir
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkArgs:--sysroot=$OUT_DIR --no-gc-sections
//#DiffEnabled:false
//#AugmentLinkerScript:linker-script-search-dir.ld
//#Archive:as(searchdir/libhidden.a),noadd:hidden.c
//#ExpectSym:hidden_from_search_dir

extern int hidden_from_search_dir;

void _start(void) {
    hidden_from_search_dir = 1;
}
