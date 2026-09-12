#[cfg(all(feature = "mimalloc", feature = "mimalloc-dynamic"))]
compile_error!("features `mimalloc` and `mimalloc-dynamic` are mutually exclusive");
#[cfg(all(feature = "mimalloc", feature = "dhat"))]
compile_error!("features `mimalloc` and `dhat` are mutually exclusive");
#[cfg(all(feature = "mimalloc-dynamic", feature = "dhat"))]
compile_error!("features `mimalloc-dynamic` and `dhat` are mutually exclusive");

#[cfg(feature = "mimalloc")]
#[global_allocator]
static MIMALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "mimalloc-dynamic")]
#[global_allocator]
static MIMALLOC: MimallocDynamic = MimallocDynamic;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "mimalloc-dynamic")]
struct MimallocDynamic;

#[cfg(feature = "mimalloc-dynamic")]
unsafe impl std::alloc::GlobalAlloc for MimallocDynamic {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        unsafe { mi_malloc_aligned(layout.size(), layout.align()).cast() }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: std::alloc::Layout) {
        unsafe { mi_free(ptr.cast()) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        unsafe { mi_realloc_aligned(ptr.cast(), new_size, layout.align()).cast() }
    }
}

#[cfg(feature = "mimalloc-dynamic")]
unsafe extern "C" {
    fn mi_malloc_aligned(size: usize, alignment: usize) -> *mut std::ffi::c_void;
    fn mi_realloc_aligned(
        p: *mut std::ffi::c_void,
        newsize: usize,
        alignment: usize,
    ) -> *mut std::ffi::c_void;
    fn mi_free(p: *mut std::ffi::c_void);
}

fn main() {
    if let Err(error) = run() {
        libwild::error::report_error_and_exit(&error)
    }
}

/// The current Wild version as written by build.rs.
const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

fn run() -> libwild::error::Result {
    #[cfg(feature = "dhat")]
    let _profiler = dhat::Profiler::new_heap();

    libwild::init_timing()?;

    let mut args = libwild::Args::new(std::env::args)?;
    args.set_version(VERSION);
    args.parse(std::env::args)?;

    if libwild::should_fork(&args) {
        // Safety: We haven't spawned any threads yet.
        unsafe { libwild::run_in_subprocess(args) };
    } else {
        // Run the linker in this process without forking.

        // Note, we need to setup tracing before worker, otherwise the threads won't contribute to
        // counters such as --time=cycles,instructions etc.
        libwild::setup_tracing(&args)?;

        libwild::run(args)
    }
}
