use super::EXPERIMENTAL_PLATFORMS;
use super::FILES_PER_GROUP_ENV;
use super::REFERENCE_LINKER_ENV;
use super::VALIDATE_ENV;
use super::WRITE_LAYOUT_ENV;
use super::WRITE_TRACE_ENV;
use super::WRITE_VERIFY_ALLOCATIONS_ENV;
use super::coff;
use super::elf;
use super::macho;
use super::wasm;
use foldhash::HashSet;
use jobserver::Acquired;
use jobserver::Client;
use rayon::ThreadPoolBuilder;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use wild_error::bail;
use wild_error::ensure;
use wild_error::env;
use wild_error::error::Result;
use wild_error::error::Warning;
use wild_fs::fs::FileReplacementMode;
use wild_fs::fs::FileWriteMode;
use wild_platform::FileId;

#[derive(derive_more::Debug)]
pub struct CommonArgs {
    pub unrecognized_options: Vec<String>,

    pub output: Arc<Path>,
    pub relocation_model: RelocationModel,

    /// The number of actually available threads (considering jobserver)
    pub available_threads: NonZeroUsize,
    pub num_threads: Option<NonZeroUsize>,
    pub files_per_group: Option<u32>,

    jobserver_client: Option<Client>,
    pub inputs: Vec<Input>,
    pub file_replacement_mode: Option<FileReplacementMode>,
    pub file_write_mode: Option<FileWriteMode>,
    pub fallocate_output_file: Option<bool>,
    pub madvise_huge_pages: Option<bool>,
    /// Input paths observed while parsing, for `WILD_SAVE_DIR` / `WILD_SAVE_BASE`.
    pub files_to_copy: HashSet<PathBuf>,

    /// Original CLI tokens after the program name, for save-dir replay scripts.
    pub cli_args: Vec<String>,

    pub prepopulate_maps: bool,
    pub debug_fuel: Option<AtomicI64>,
    pub should_fork: bool,
    pub demangle: bool,
    pub validate_output: bool,
    pub verify_allocation_consistency: bool,
    pub write_layout: bool,
    pub write_trace: bool,
    pub experimental_platforms: bool,
    pub print_allocations: Option<FileId>,
    pub sym_info: Option<String>,
    pub numeric_experiments: Vec<Option<u64>>,
    pub version_mode: VersionMode,

    /// If `Some`, then we'll time how long each phase takes. We'll also measure the specified
    /// counters, if any.
    pub time_phase_options: Option<Vec<CounterKind>>,

    /// Warnings that we encountered either during argument parsing, or during subsequent linker
    /// execution based on those arguments.
    #[debug(skip)]
    pub warning_callback: Box<WarningCallback>,

    /// The version of the linker being used.
    pub version: std::borrow::Cow<'static, str>,

    pub(super) has_flavor: bool,

    /// When set, Wild writes a `{output}.incr` state directory and pads output sections so a later
    /// `--incremental` link can patch in place. Falls back to a full link when LTO, GC, or
    /// strict-order sections (`.init` / `.fini`) are involved.
    pub incremental: bool,
}

pub type WarningCallback = dyn Fn(Warning) + Send + Sync + 'static;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionMode {
    /// Don't print version
    None,
    /// Print version and continue linking if object files are specified (-v).
    Verbose,
    /// Print version along with supported emulations and continue linking if object files are
    /// specified (-V).
    VerboseWithEmulations,
    /// Print version and exit immediately (--version)
    ExitAfterPrint,
}

#[derive(Debug, Clone, Copy)]
pub enum CounterKind {
    Cycles,
    Instructions,
    CacheMisses,
    BranchMisses,
    PageFaults,
    PageFaultsMinor,
    PageFaultsMajor,
    L1dRead,
    L1dMiss,
}

use wild_platform::RelocationModel;

pub trait HasCommonArgs {
    fn common(&self) -> &CommonArgs;
    fn common_mut(&mut self) -> &mut CommonArgs;
}

impl Default for CommonArgs {
    fn default() -> Self {
        Self {
            output: Arc::from(Path::new("a.out")),
            relocation_model: RelocationModel::Fixed,
            available_threads: NonZeroUsize::new(1).unwrap(),
            num_threads: None,
            jobserver_client: None,
            files_per_group: None,
            inputs: Vec::new(),
            file_replacement_mode: None,
            unrecognized_options: Vec::new(),
            files_to_copy: HashSet::default(),
            cli_args: Vec::new(),
            file_write_mode: None,
            fallocate_output_file: None,
            madvise_huge_pages: None,
            prepopulate_maps: false,
            debug_fuel: None,
            should_fork: true,
            demangle: true,
            version_mode: VersionMode::None,
            validate_output: env::var(VALIDATE_ENV).is_ok_and(|v| v == "1"),
            verify_allocation_consistency: env::var(WRITE_VERIFY_ALLOCATIONS_ENV)
                .is_ok_and(|v| v == "1"),
            write_layout: env::var(WRITE_LAYOUT_ENV).is_ok_and(|v| v == "1"),
            write_trace: env::var(WRITE_TRACE_ENV).is_ok_and(|v| v == "1"),
            print_allocations: env::var("WILD_PRINT_ALLOCATIONS")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(FileId::from_encoded),
            experimental_platforms: env::var(EXPERIMENTAL_PLATFORMS).is_ok_and(|v| v == "1"),
            numeric_experiments: Vec::new(),
            sym_info: None,
            time_phase_options: None,
            warning_callback: Box::new(default_warning_callback),
            version: std::borrow::Cow::Borrowed("unknown version"),
            has_flavor: false,
            incremental: env::var("WILD_INCREMENTAL").is_ok_and(|v| v == "1"),
        }
    }
}

fn default_warning_callback(warning: Warning) {
    eprintln!("{warning}");
    // Suppress clippy warning. We need to confirm to an API that takes warning by value.
    drop(warning);
}

impl CommonArgs {
    pub fn report_unrecognized(&self) -> Result {
        if !self.unrecognized_options.is_empty() {
            let options_list = self.unrecognized_options.join(", ");
            bail!("unrecognized option(s): {}", options_list);
        }

        Ok(())
    }

    /// Builds up the thread pool, using the explicit number of threads if specified,
    /// or falling back to the jobserver protocol if available.
    ///
    /// <https://www.gnu.org/software/make/manual/html_node/POSIX-Jobserver.html>
    pub fn build_thread_pool(&mut self) -> Result<ThreadPool> {
        let _span = tracing::info_span!("Build thread pool").entered();

        let mut tokens = Vec::new();
        self.available_threads = self.num_threads.unwrap_or_else(|| {
            if let Some(client) = &self.jobserver_client {
                while let Ok(Some(acquired)) = client.try_acquire() {
                    tokens.push(acquired);
                }
                tracing::trace!(count = tokens.len(), "Acquired jobserver tokens");
                // Our parent "holds" one jobserver token, add it.
                NonZeroUsize::new(tokens.len() + 1).unwrap()
            } else {
                std::thread::available_parallelism().unwrap_or(NonZeroUsize::new(1).unwrap())
            }
        });

        // Always let Rayon spawn the pool's workers, even when only one thread is requested.
        // Reusing the current thread would fail if it already belonged to another pool; instead,
        // it blocks in `install` until linking finishes.
        let pool = ThreadPoolBuilder::new()
            .num_threads(self.available_threads.get())
            .build()?;

        Ok(ThreadPool {
            pool,
            _jobserver_tokens: tokens,
        })
    }

    /// Binutils feature level advertised in `--version`. Glibc `configure`, the
    /// kernel's `scripts/ld-version.sh`, and GCC all require a `GNU ld` line with
    /// a dotted version; 2.39 is glibc's minimum.
    pub const GNU_LD_COMPAT_VERSION: &str = "2.44";

    /// Returns a string that identifies this linker. This is written into the .comment
    /// section which usually also contains the versions of compilers that were used.
    pub fn linker_identity(&self) -> String {
        format!("Wild {} (compatible with GNU linkers)", self.version)
    }

    /// `--version` / `-v` / `-V` text. First line matches GNU ld so glibc and the
    /// kernel accept Wild. The parenthetical must not contain a `x.y` version or
    /// glibc's `sed` captures that instead of `GNU_LD_COMPAT_VERSION`.
    pub fn version_message(&self) -> String {
        format!(
            "GNU ld (Wild) {}\n{}",
            Self::GNU_LD_COMPAT_VERSION,
            self.linker_identity()
        )
    }

    /// Records an input path for later `WILD_SAVE_DIR` / `WILD_SAVE_BASE` copying.
    pub fn handle_file(&mut self, arg: &str) {
        self.files_to_copy.insert(Path::new(arg).to_path_buf());
    }

    /// Adds a linker script to our outputs. Note, this is only called for scripts specified via
    /// flags like -T. Where a linker script is just listed as an argument, this won't be called.
    pub fn add_script(&mut self, path: &str) {
        self.inputs.push(Input {
            spec: InputSpec::File(Box::from(Path::new(path))),
            search_first: None,
            modifiers: Modifiers::default(),
        });
    }

    /// Uses 1 debug fuel, returning how much fuel remains. Debug fuel is intended to be used when
    /// debugging certain kinds of bugs, so this function isn't normally referenced. To use it, the
    /// caller should take a different branch depending on whether the value is still positive. You
    /// can then do a binary search.
    pub fn use_debug_fuel(&self) -> i64 {
        let Some(fuel) = self.debug_fuel.as_ref() else {
            return i64::MAX;
        };
        fuel.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) - 1
    }

    /// Returns whether there was sufficient fuel. If the last bit of fuel was used, then calls
    /// `last_cb`.
    #[allow(unused)]
    pub fn use_debug_fuel_on_last(&self, last_cb: impl FnOnce()) -> bool {
        match self.use_debug_fuel() {
            1.. => true,
            0 => {
                last_cb();
                true
            }
            _ => false,
        }
    }

    pub fn should_fork(&self) -> bool {
        self.should_fork
    }

    pub fn numeric_experiment(&self, exp: wild_platform::Experiment, default: u64) -> u64 {
        self.numeric_experiments
            .get(exp as usize)
            .copied()
            .flatten()
            .unwrap_or(default)
    }

    pub fn from_env() -> Result<Self> {
        use wild_platform::MAX_FILES_PER_GROUP;

        // SAFETY: Should be called early before other descriptors are opened and
        // so we open it before the arguments are parsed (can open a file).
        let jobserver_client = unsafe { Client::from_env() };

        let files_per_group = env::var(FILES_PER_GROUP_ENV)
            .ok()
            .map(|s| s.parse())
            .transpose()?;

        if let Some(x) = files_per_group {
            ensure!(
                x <= MAX_FILES_PER_GROUP,
                "{FILES_PER_GROUP_ENV}={x} but maximum is {MAX_FILES_PER_GROUP}"
            );
        }

        let mut common = Self {
            files_per_group,
            jobserver_client,
            ..Default::default()
        };

        if env::var(REFERENCE_LINKER_ENV).is_ok() {
            common.write_layout = true;
            common.write_trace = true;
        }

        Ok(common)
    }
}

/// The thread pool used by the linker. If a jobserver is being used, dropping this instance will
/// release jobserver tokens.
pub struct ThreadPool {
    pub pool: rayon::ThreadPool,
    _jobserver_tokens: Vec<Acquired>,
}

// TODO: remove
#[allow(clippy::large_enum_variant)]
pub enum Args {
    Coff(coff::CoffArgs),
    Elf(elf::ElfArgs),
    MachO(macho::MachOArgs),
    Wasm(wasm::WasmArgs),
}

impl std::fmt::Debug for Args {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Args::Coff(args) => args.fmt(f),
            Args::Elf(args) => args.fmt(f),
            Args::MachO(args) => args.fmt(f),
            Args::Wasm(args) => args.fmt(f),
        }
    }
}

pub use wild_scripts::Input;
pub use wild_scripts::InputSpec;
pub use wild_scripts::Modifiers;

#[derive(Debug, Eq, PartialEq)]
pub enum BSymbolicKind {
    None,
    All,
    Functions,
    NonWeakFunctions,
    NonWeak,
}

pub fn parse_time_phase_options(input: &str) -> Result<Vec<CounterKind>> {
    input.split(',').map(|s| s.parse()).collect()
}

impl std::str::FromStr for CounterKind {
    type Err = wild_error::error::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "cycles" => CounterKind::Cycles,
            "instructions" => CounterKind::Instructions,
            "cache-misses" => CounterKind::CacheMisses,
            "branch-misses" => CounterKind::BranchMisses,
            "page-faults" => CounterKind::PageFaults,
            "page-faults-minor" => CounterKind::PageFaultsMinor,
            "page-faults-major" => CounterKind::PageFaultsMajor,
            "l1d-read" => CounterKind::L1dRead,
            "l1d-miss" => CounterKind::L1dMiss,
            other => bail!("Unsupported performance counter `{other}`"),
        })
    }
}
