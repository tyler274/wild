// GNU `HIDDEN(sym = expr)` is an assignment that is not exported from a DSO.
//
//#Mode:dynamic
//#RunEnabled:false
//#LinkArgs:-shared -T ./linker-script-hidden.ld
//#CompArgs:-fPIC
//#ExpectSym:hidden_abs address=0x1234
//#ExpectSym:visible_abs address=0x5678
//#ExpectDynSym:visible_abs address=0x5678
//#NoDynSym:hidden_abs
//#ExpectSym:_hidden_start
//#ExpectSym:visible_end
//#NoDynSym:_hidden_start
//#ExpectDynSym:visible_end
//#DiffIgnore:.dynamic.*
//#DiffIgnore:section.got
//#DiffIgnore:section.rela.dyn
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

extern char hidden_abs;
extern char visible_abs;
extern char _hidden_start;
extern char visible_end;

void *keep(void) {
    return &hidden_abs + (long)&visible_abs + (long)&_hidden_start + (long)&visible_end;
}
