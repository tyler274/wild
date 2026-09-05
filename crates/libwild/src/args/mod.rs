//! A handwritten parser for our arguments.
//!
//! We don't currently use a 3rd party library like clap for a few reasons. Firstly, we need to
//! support flags like `--push-state` and `--pop-state`. These need to push and pop a state stack
//! when they're parsed. Some of the other flags then need to manipulate the state of the top of the
//! stack. Positional arguments like input files and libraries to link, then need to have the
//! current state of the stack attached to that file.
//!
//! Secondly, long arguments need to also be accepted with a single '-' in addition to the more
//! common double-dash.
//!
//! Basically, we need to be able to parse arguments in the same way as the other linkers on the
//! platform that we're targeting.

use crate::bail;
use crate::error::Context;
use crate::error::Result;
#[allow(unused_imports)]
pub(crate) use crate::fs::FileReplacementMode;
use crate::platform::Args as _;
use crate::save_dir::SaveDir;
use crate::timing_phase;
use std::io::Write;
use std::path::Path;

macro_rules! impl_platform_args_from_common {
    () => {
        fn output(&self) -> &std::sync::Arc<std::path::Path> {
            &self.common.output
        }

        fn relocation_model(&self) -> crate::platform::RelocationModel {
            self.common.relocation_model
        }

        fn warning(&self, message: impl Into<String>) {
            (self.common.warning_callback)(crate::error::Warning::new(message.into()));
        }

        fn incremental(&self) -> bool {
            self.common.incremental
        }

        fn available_threads(&self) -> std::num::NonZeroUsize {
            self.common.available_threads
        }

        fn demangle(&self) -> bool {
            self.common.demangle
        }

        fn validate_output(&self) -> bool {
            self.common.validate_output
        }

        fn write_layout(&self) -> bool {
            self.common.write_layout
        }

        fn write_trace(&self) -> bool {
            self.common.write_trace
        }

        fn experimental_platforms(&self) -> bool {
            self.common.experimental_platforms
        }

        fn prepopulate_maps(&self) -> bool {
            self.common.prepopulate_maps
        }

        fn verify_allocation_consistency(&self) -> bool {
            self.common.verify_allocation_consistency
        }

        fn files_per_group(&self) -> Option<u32> {
            self.common.files_per_group
        }

        fn file_replacement_mode(&self) -> Option<crate::fs::FileReplacementMode> {
            self.common.file_replacement_mode
        }

        fn file_write_mode(&self) -> Option<crate::fs::FileWriteMode> {
            self.common.file_write_mode
        }

        fn fallocate_output_file(&self) -> Option<bool> {
            self.common.fallocate_output_file
        }

        fn madvise_huge_pages(&self) -> Option<bool> {
            self.common.madvise_huge_pages
        }

        fn linker_identity(&self) -> String {
            self.common.linker_identity()
        }

        fn numeric_experiment(&self, exp: crate::platform::Experiment, default: u64) -> u64 {
            self.common.numeric_experiment(exp, default)
        }

        fn symbol_info_query(&self) -> Option<&str> {
            self.common.sym_info.as_deref()
        }

        fn should_trace_file(&self, file_id: crate::input_data::FileId) -> bool {
            self.common.print_allocations == Some(file_id)
        }
    };
}
pub(crate) use impl_platform_args_from_common;

pub mod coff;
pub mod elf;
pub mod macho;
pub mod wasm;

mod declare;
pub(crate) mod parse;
pub(crate) mod types;

pub(crate) use crate::platform::CopyRelocations;
pub(crate) use crate::platform::CopyRelocationsDisabledReason;
pub(crate) use crate::platform::Experiment;
pub(crate) use crate::platform::RelocationModel;
pub(crate) use crate::platform::UnresolvedSymbols;
#[allow(unused_imports)]
pub(crate) use declare::*;
#[allow(unused_imports)]
pub(crate) use parse::*;
#[allow(unused_imports)]
pub use types::*;

pub(crate) const FILES_PER_GROUP_ENV: &str = "WILD_FILES_PER_GROUP";
pub const REFERENCE_LINKER_ENV: &str = "WILD_REFERENCE_LINKER";
pub const VALIDATE_ENV: &str = "WILD_VALIDATE_OUTPUT";
pub const WILD_UNSUPPORTED_ENV: &str = crate::platform::WILD_UNSUPPORTED_ENV;
pub const WRITE_LAYOUT_ENV: &str = "WILD_WRITE_LAYOUT";
pub const WRITE_TRACE_ENV: &str = "WILD_WRITE_TRACE";
pub const EXPERIMENTAL_PLATFORMS: &str = "WILD_EXPERIMENTAL_PLATFORMS";

/// Set this environment variable if you get a failure during writing due to too much or too little
/// space being allocated to some section. When set, each time we allocate during layout, we'll
/// check that what we're doing is consistent with writing and fail in a more easy to debug way. i.e
/// we'll report the particular combination of value flags, resolution flags etc that triggered the
/// inconsistency.
pub(crate) const WRITE_VERIFY_ALLOCATIONS_ENV: &str = "WILD_VERIFY_ALLOCATIONS";

impl Args {
    /// Construct a new instance, but doesn't yet parse the arguments. The supplied arguments are
    /// only used to help decide what kind of argument parsing we'll be doing - i.e. what platform
    /// we're linking for. We split into two phases so that the caller can adjust defaults before
    /// the actual parsing occurs.
    pub fn new<F, S, I>(input: F) -> Result<Self>
    where
        F: Fn() -> I,
        S: AsRef<str>,
        I: Iterator<Item = S>,
    {
        let mut input = input();

        let prog_name = input.next().context("Missing argument 0 (program name)")?;

        let mut has_flavor = false;

        let platform = if input.next().is_some_and(|arg| arg.as_ref() == "-flavor") {
            has_flavor = true;

            let flavor = input
                .next()
                .context("-flavor requires an argument (gnu, darwin, or link)")?;

            PlatformKind::from_flavor(flavor.as_ref())?
        } else if let Some(platform) = PlatformKind::from_executable_name(prog_name.as_ref()) {
            platform
        } else {
            PlatformKind::host()
        };

        let mut args = match platform {
            PlatformKind::Coff => Args::Coff(coff::CoffArgs::new()?),
            PlatformKind::Elf => Args::Elf(elf::ElfArgs::new()?),
            PlatformKind::MachO => Args::MachO(macho::MachOArgs::new()?),
            PlatformKind::Wasm => Args::Wasm(wasm::WasmArgs::new()?),
        };

        // Store whether we got a flavor arg to make parsing simpler.
        args.common_mut().has_flavor = has_flavor;

        Ok(args)
    }

    /// Parse CLI arguments. Runs format-specific parser based on the host target.
    pub fn parse<F: Fn() -> I, S: AsRef<str>, I: Iterator<Item = S>>(
        &mut self,
        input: F,
    ) -> Result {
        timing_phase!("Parse args");

        self.common_mut().save_dir = SaveDir::new(input())?;

        let mut input = input();

        // Skip the program name.
        input.next();

        if self.common().has_flavor {
            input.next();
            input.next();
        }

        match self {
            Args::Coff(args) => args.parse(input),
            Args::Elf(args) => args.parse(input),
            Args::MachO(args) => args.parse(input),
            Args::Wasm(args) => args.parse(input),
        }
    }

    /// Set the version identifier of the linker.
    pub fn set_version(&mut self, version: &str) {
        self.common_mut().version = std::borrow::Cow::Owned(version.to_owned());
    }

    /// Calls the callback whenever a warning is emitted. The default, if this method is never
    /// called is to print the warning to stderr. Calling this method suppresses the default
    /// behaviour. Warnings may be emitted while parsing arguments or later, while using the
    /// arguments to link. As such, this method should be called after calling `new` and before
    /// calling `parse`.
    pub fn on_warning(&mut self, warning_callback: Box<WarningCallback>) {
        self.common_mut().warning_callback = Box::new(warning_callback);
    }

    pub(crate) fn common(&self) -> &CommonArgs {
        match self {
            Args::Coff(coff_args) => &coff_args.common,
            Args::Elf(elf_args) => &elf_args.common,
            Args::MachO(macho_args) => &macho_args.common,
            Args::Wasm(wasm_args) => &wasm_args.common,
        }
    }

    pub(crate) fn common_mut(&mut self) -> &mut CommonArgs {
        match self {
            Args::Coff(coff_args) => &mut coff_args.common,
            Args::Elf(elf_args) => &mut elf_args.common,
            Args::MachO(macho_args) => &mut macho_args.common,
            Args::Wasm(wasm_args) => &mut wasm_args.common,
        }
    }

    pub(crate) fn print_emulation_info(&self, stdout: &mut dyn Write) -> Result<()> {
        match self {
            Args::Elf(_) => {
                writeln!(
                    stdout,
                    "supported emulations: {}",
                    elf::supported_emulations()
                )?;
            }
            Args::Coff(_) | Args::MachO(_) | Args::Wasm(_) => (),
        }
        Ok(())
    }
}

enum PlatformKind {
    Coff,
    Elf,
    MachO,
    Wasm,
}

impl PlatformKind {
    fn host() -> Self {
        if cfg!(target_os = "macos") {
            PlatformKind::MachO
        } else {
            PlatformKind::Elf
        }
    }

    fn from_flavor(flavor: &str) -> Result<Self> {
        match flavor {
            "gnu" | "ld" => Ok(PlatformKind::Elf),
            "darwin" | "ld64" => Ok(PlatformKind::MachO),
            "link" => Ok(PlatformKind::Coff),
            "wasm" | "ld-wasm" => Ok(PlatformKind::Wasm),
            _ => bail!(
                "Unknown flavor '{}'. Valid flavors: gnu, darwin, link",
                flavor
            ),
        }
    }

    fn from_executable_name(name: &str) -> Option<Self> {
        let base_name = Path::new(name).file_stem().and_then(|n| n.to_str())?;

        // MSVC-world tools are commonly invoked as `LINK.EXE`, so match those names without regard
        // to case.
        if base_name.eq_ignore_ascii_case("link") || base_name.eq_ignore_ascii_case("lld-link") {
            return Some(PlatformKind::Coff);
        }

        match base_name {
            "ld" => Some(PlatformKind::Elf),
            "ld64" => Some(PlatformKind::MachO),
            "ld-wasm" | "wasm-ld" => Some(PlatformKind::Wasm),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn test_flavor() {
        let args = Args::new(|| ["ld.wild"].into_iter()).unwrap();
        assert_matches!(args, Args::Elf(_));

        let args = Args::new(|| ["ld64.wild"].into_iter()).unwrap();
        assert_matches!(args, Args::MachO(_));

        let mut args = Args::new(|| ["wild", "-flavor", "gnu"].into_iter()).unwrap();
        assert_matches!(args, Args::Elf(_));
        args.parse(|| ["wild", "-flavor", "gnu"].into_iter())
            .unwrap();
        assert_eq!(args.common().inputs, []);

        let args = Args::new(|| ["wild", "-flavor", "darwin"].into_iter()).unwrap();
        assert_matches!(args, Args::MachO(_));

        // -flavor has priority
        let args = Args::new(|| ["ld.wild", "-flavor", "darwin"].into_iter()).unwrap();
        assert_matches!(args, Args::MachO(_));

        let args = Args::new(|| ["ld64.wild", "-flavor", "gnu"].into_iter()).unwrap();
        assert_matches!(args, Args::Elf(_));

        let args = Args::new(|| ["wild", "-flavor", "link"].into_iter()).unwrap();
        assert_matches!(args, Args::Coff(_));

        assert!(Args::new(|| ["ld.wild", "-flavor", "invalid"].into_iter()).is_err());
        assert!(Args::new(|| ["ld.wild", "-flavor"].into_iter()).is_err());
    }
}
