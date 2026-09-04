// rustc + clang driver (WILD_SAVE_DIR). `--cfg wild_inc` changes CGU hashes, so
// the second link is allowed to fall back to a full padded incremental link.

//#AbstractConfig:base
//#SkipArch:ppc64le
//#Cross:false
//#SkipLinker:ld
//#SkipLinker:lld
//#SkipLinker:mold
//#SkipLinker:gold
//#DiffEnabled:false
//#TestIncremental:true
//#IncrementalAllowFallback:true
//#CompArgs:-C opt-level=0 -C link-arg=-Wl,--incremental

//#Config:opt0:base

//#Config:opt1:base
//#CompArgs:-C opt-level=1 -C link-arg=-Wl,--incremental

//#Config:opt2:base
//#CompArgs:-C opt-level=2 -C link-arg=-Wl,--incremental

//#Config:opt3:base
//#CompArgs:-C opt-level=3 -C link-arg=-Wl,--incremental

//#Config:opts:base
//#CompArgs:-C opt-level=s -C link-arg=-Wl,--incremental

fn main() {
    let code = if cfg!(wild_inc) { 43 } else { 42 };
    std::process::exit(code);
}
