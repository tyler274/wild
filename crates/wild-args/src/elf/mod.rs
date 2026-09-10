//! An elf-specific extension of `super::Args` and parsing implementation to match gnu style
//! linkers.

mod parser;

use super::BSymbolicKind;
use crate::alignment::Alignment;
use crate::arch::Architecture;
use crate::args::CommonArgs;
use crate::args::CopyRelocations;
use crate::args::CopyRelocationsDisabledReason;
use crate::args::HasCommonArgs as _;
use crate::args::Modifiers;
use crate::args::UnresolvedSymbols;
use crate::args::parse_number;
use crate::bail;
use crate::error::Result;
use crate::output_kind::OutputKind;
use crate::output_section_id::SectionName;
use crate::platform;
use crate::platform::Args as _;
use hashbrown::HashMap;
use hashbrown::HashSet;
use indexmap::IndexSet;
use itertools::Itertools;
use object::Endianness;
use parser::IGNORED_FLAGS;
use parser::setup_argument_parser;
use std::ffi::CString;
use std::num::NonZeroU32;
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use strum::EnumMessage as _;
use strum::IntoEnumIterator as _;

#[derive(Debug)]
pub struct ElfArgs {
    pub(crate) common: super::CommonArgs,

    emulation: Emulation,
    pub(crate) lib_search_path: Vec<Box<Path>>,
    dynamic_linker: DynamicLinker,
    pub(crate) strip: Strip,
    pub(crate) merge_sections: bool,
    pub(crate) version_script_path: Option<PathBuf>,
    pub(crate) should_write_eh_frame_hdr: bool,
    pub(crate) wrap: Vec<String>,
    pub(crate) rpath: Option<String>,
    pub(crate) soname: Option<String>,
    pub(crate) exclude_libs: ExcludeLibs,
    pub(crate) gc_sections: bool,
    pub(crate) build_id: BuildIdOption,

    // Whether to emit errors if our input objects have undefined symbols that we can't resolve. If
    // not specified, then the behaviour depends on whether we're emitting a shared object or an
    // executable.
    pub(crate) no_undefined: Option<bool>,

    pub(crate) allow_shlib_undefined: bool,
    pub(crate) needs_origin_handling: bool,
    pub(crate) needs_nodelete_handling: bool,
    pub(crate) copy_relocations: CopyRelocations,
    pub(crate) sysroot: Option<Box<Path>>,
    pub(crate) undefined: Vec<String>,
    pub(crate) relro: bool,
    pub(crate) entry: Option<String>,
    pub(crate) export_all_dynamic_symbols: bool,
    pub(crate) export_list: Vec<String>,
    pub(crate) export_list_path: Option<PathBuf>,
    pub(crate) auxiliary: Vec<String>,
    pub(crate) enable_new_dtags: bool,
    pub(crate) plugin_path: Option<String>,
    pub(crate) plugin_args: Vec<CString>,

    /// Symbol definitions from `--defsym` options. Each entry is (symbol_name, value_or_symbol).
    pub(crate) defsym: Vec<(String, String)>,

    /// Section start addresses from `--section-start` options. Maps section name to address.
    pub(crate) section_start: HashMap<Vec<u8>, u64>,

    /// Segment start address overrides from `-Ttext`, `-Tdata`, `-Tbss`.
    /// Used to implement `SEGMENT_START("name", default)` per GNU ld behaviour.
    pub(crate) ttext: Option<u64>,
    pub(crate) tdata: Option<u64>,
    pub(crate) tbss: Option<u64>,

    /// If set, GC stats will be written to the specified filename.
    pub(crate) write_gc_stats: Option<PathBuf>,

    /// If set, and we're writing GC stats, then ignore any input files that contain any of the
    /// specified substrings.
    pub(crate) gc_stats_ignore: Vec<String>,

    pub(crate) verbose_gc_stats: bool,

    pub(crate) dependency_file: Option<PathBuf>,
    pub(crate) execstack: bool,
    pub(crate) got_plt_syms: bool,
    pub(crate) b_symbolic: BSymbolicKind,
    pub(crate) relax: bool,
    pub(crate) should_write_linker_identity: bool,
    pub(crate) hash_style: HashStyle,
    pub(crate) unresolved_symbols: UnresolvedSymbols,
    pub(crate) error_unresolved_symbols: bool,
    pub(crate) allow_multiple_definitions: bool,
    pub(crate) z_interpose: bool,
    pub(crate) z_isa: Option<NonZeroU32>,
    pub(crate) z_stack_size: Option<NonZeroU64>,
    pub(crate) z_pack_relative_relocs: bool,
    pub(crate) max_page_size: Option<Alignment>,
    pub(crate) common_page_size: Option<Alignment>,
    pub(crate) trace: bool,
    pub(crate) pack_dyn_relocs: PackDynRelocs,
    pub(crate) use_android_relr_tags: bool,
    pub(crate) discard_sframe: bool,

    pub(crate) should_output_executable: bool,
    pub(crate) should_output_partial_object: bool,
    pub(crate) emit_relocs: bool,
    pub(crate) discard_none: bool,

    pub(crate) nmagic: bool,
    pub(crate) rosegment: bool,
    pub(crate) gdb_index: bool,

    pub(crate) rpath_set: IndexSet<String>,

    pub(crate) experimental_sframe: bool,

    pub(crate) debug_compression_kind: Option<CompressionKind>,
    pub(crate) sort_section: Option<SortSectionMode>,
    pub(crate) output_format_endian: Option<Endianness>,
    pub(crate) orphan_handling: crate::platform::OrphanHandling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortSectionMode {
    Name,
    Alignment,
}

#[derive(Debug)]
pub(crate) enum Strip {
    Nothing,
    Debug,
    All,
    Retain(HashSet<Vec<u8>>),
}

#[derive(Debug)]
pub(crate) enum BuildIdOption {
    None,
    Fast,
    Hex(Vec<u8>),
    Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashStyle {
    Gnu,
    Sysv,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExcludeLibs {
    None,
    All,
    Some(HashSet<Box<str>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackDynRelocs {
    None,
    Android,
    AndroidRelr,
    Relr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompressionKind {
    Zlib,
    Zstd,
}

impl ExcludeLibs {
    pub(crate) fn should_exclude(&self, lib_path: &[u8]) -> bool {
        match self {
            ExcludeLibs::None => false,
            ExcludeLibs::All => true,
            ExcludeLibs::Some(libs) => {
                let lib_path_str = String::from_utf8_lossy(lib_path);
                let lib_name = lib_path_str.rsplit('/').next().unwrap_or(&lib_path_str);

                libs.contains(lib_name)
            }
        }
    }
}

impl HashStyle {
    pub(crate) const fn includes_gnu(self) -> bool {
        matches!(self, HashStyle::Gnu | HashStyle::Both)
    }

    pub(crate) const fn includes_sysv(self) -> bool {
        matches!(self, HashStyle::Sysv | HashStyle::Both)
    }
}

#[derive(Debug, Default)]
enum DynamicLinker {
    #[default]
    EmulationDefault,
    Omit,
    Explicit(Box<Path>),
}

#[derive(Debug, Clone, Copy, strum::EnumIter, strum::EnumMessage, strum::EnumString)]
enum Emulation {
    #[strum(serialize = "elf_x86_64", message = "x86-64 ELF target")]
    ElfX86_64,
    #[strum(serialize = "elf_x86_64_sol2", message = "x86-64 ELF target (Solaris)")]
    ElfX86_64Sol2,
    #[strum(
        serialize = "aarch64elf",
        serialize = "aarch64linux",
        message = "AArch64 ELF target"
    )]
    AArch64,
    #[strum(serialize = "elf64lriscv", message = "RISC-V 64-bit ELF target")]
    RiscV64,
    #[strum(serialize = "elf64loongarch", message = "LoongArch 64-bit ELF target")]
    LoongArch64,
    #[strum(serialize = "elf64lppc", message = "PowerPC64 LE ELF target")]
    Ppc64,
    #[strum(disabled)]
    Unsupported,
}

impl Emulation {
    fn architecture(self) -> Architecture {
        match self {
            Emulation::ElfX86_64 | Emulation::ElfX86_64Sol2 => Architecture::X86_64,
            Emulation::AArch64 => Architecture::AArch64,
            Emulation::RiscV64 => Architecture::RiscV64,
            Emulation::LoongArch64 => Architecture::LoongArch64,
            Emulation::Ppc64 => Architecture::Ppc64,
            Emulation::Unsupported => Architecture::Unsupported,
        }
    }

    fn default_dynamic_linker(self) -> Option<&'static Path> {
        match self {
            Emulation::ElfX86_64Sol2 => Some(Path::new("/lib/amd64/ld.so.1")),
            _ => None,
        }
    }
}

impl Default for ElfArgs {
    fn default() -> Self {
        Self {
            common: CommonArgs::default(),

            emulation: default_emulation(),

            lib_search_path: Vec::new(),
            should_output_executable: true,
            should_output_partial_object: false,
            emit_relocs: false,
            discard_none: false,
            dynamic_linker: DynamicLinker::default(),
            strip: Strip::Nothing,
            // For now, we default to --gc-sections. This is different to other linkers, but other
            // than being different, there doesn't seem to be any downside to doing
            // this. We don't currently do any less work if we're not GCing sections,
            // but do end up writing more, so --no-gc-sections will almost always be as
            // slow or slower than --gc-sections. For that reason, the latter is
            // probably a good default.
            gc_sections: true,
            merge_sections: true,
            copy_relocations: CopyRelocations::Allowed,
            version_script_path: None,
            should_write_eh_frame_hdr: false,
            write_gc_stats: None,
            wrap: Vec::new(),
            gc_stats_ignore: Vec::new(),
            verbose_gc_stats: false,
            rpath: None,
            soname: None,
            enable_new_dtags: true,
            execstack: false,
            needs_origin_handling: false,
            needs_nodelete_handling: false,
            should_write_linker_identity: true,
            build_id: BuildIdOption::None,
            exclude_libs: ExcludeLibs::None,
            no_undefined: None,
            allow_shlib_undefined: false,
            sysroot: None,
            dependency_file: None,
            undefined: Vec::new(),
            relro: true,
            entry: None,
            b_symbolic: BSymbolicKind::None,
            export_all_dynamic_symbols: false,
            export_list: Vec::new(),
            export_list_path: None,
            defsym: Vec::new(),
            section_start: HashMap::new(),
            ttext: None,
            tdata: None,
            tbss: None,
            got_plt_syms: false,
            relax: true,
            hash_style: HashStyle::Both,
            trace: false,
            pack_dyn_relocs: PackDynRelocs::None,
            use_android_relr_tags: false,
            discard_sframe: false,

            nmagic: false,
            rosegment: true,

            unresolved_symbols: UnresolvedSymbols::ReportAll,
            error_unresolved_symbols: true,
            allow_multiple_definitions: false,
            z_interpose: false,
            z_stack_size: None,
            z_isa: None,
            z_pack_relative_relocs: false,
            max_page_size: None,
            common_page_size: None,
            auxiliary: Vec::new(),
            rpath_set: Default::default(),
            plugin_path: None,
            plugin_args: Vec::new(),

            experimental_sframe: false,
            debug_compression_kind: None,
            sort_section: None,
            gdb_index: false,
            output_format_endian: None,
            orphan_handling: crate::platform::OrphanHandling::Place,
        }
    }
}

const fn default_emulation() -> Emulation {
    // We default to targeting the architecture that we're running on. We don't support running on
    // architectures that we can't target.
    #[cfg(target_arch = "x86_64")]
    {
        return Emulation::ElfX86_64;
    }
    #[cfg(target_arch = "aarch64")]
    {
        return Emulation::AArch64;
    }
    #[cfg(target_arch = "riscv64")]
    {
        return Emulation::RiscV64;
    }
    #[cfg(target_arch = "loongarch64")]
    {
        return Emulation::LoongArch64;
    }
    #[cfg(all(target_arch = "powerpc64", target_endian = "little"))]
    {
        return Emulation::Ppc64;
    }

    #[allow(unreachable_code)]
    Emulation::Unsupported
}

impl ElfArgs {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            common: CommonArgs::from_env()?,
            ..Default::default()
        })
    }

    pub(crate) fn is_relr_enabled(&self) -> bool {
        self.z_pack_relative_relocs
            || self.pack_dyn_relocs == PackDynRelocs::Relr
            || self.pack_dyn_relocs == PackDynRelocs::AndroidRelr
    }

    pub(crate) fn architecture(&self) -> Architecture {
        self.emulation.architecture()
    }

    #[cfg(test)]
    pub(crate) fn set_architecture(&mut self, architecture: Architecture) {
        self.emulation = match architecture {
            Architecture::X86_64 => Emulation::ElfX86_64,
            Architecture::AArch64 => Emulation::AArch64,
            Architecture::RiscV64 => Emulation::RiscV64,
            Architecture::LoongArch64 => Emulation::LoongArch64,
            Architecture::Ppc64 => Emulation::Ppc64,
            Architecture::Unsupported => Emulation::Unsupported,
        };
    }
}

fn set_command_line_emulation(
    args: &mut ElfArgs,
    _modifier_stack: &mut Vec<Modifiers>,
    emulation: &str,
) {
    args.emulation = emulation
        .parse()
        .expect("registered emulation should always parse");
}

fn emulations() -> impl Iterator<Item = (Emulation, &'static str)> {
    Emulation::iter().flat_map(|emulation| {
        emulation
            .get_serializations()
            .iter()
            .map(move |&name| (emulation, name))
    })
}

pub(crate) fn supported_emulations() -> String {
    emulations().map(|(_, name)| name).join(" ")
}

// Parse the supplied input arguments, which should not include the program name.
pub(crate) fn parse<S: AsRef<str>, I: Iterator<Item = S>>(
    args: &mut ElfArgs,
    mut input: I,
) -> Result {
    let mut modifier_stack = vec![Modifiers::default()];

    let arg_parser = setup_argument_parser();
    while let Some(arg) = input.next() {
        let arg = arg.as_ref();

        arg_parser.handle_argument(args, &mut modifier_stack, arg, &mut input)?;
    }

    // Copy relocations are only permitted when building executables.
    if !args.should_output_executable {
        args.copy_relocations =
            CopyRelocations::Disallowed(CopyRelocationsDisabledReason::SharedObject);
    }

    if !args.rpath_set.is_empty() {
        args.rpath = Some(std::mem::take(&mut args.rpath_set).into_iter().join(":"));
    }

    args.common.report_unrecognized()?;

    if !args.auxiliary.is_empty() && args.should_output_executable {
        bail!("-f may not be used without -shared");
    }

    if args.pack_dyn_relocs == PackDynRelocs::Android
        || args.pack_dyn_relocs == PackDynRelocs::AndroidRelr
    {
        args.warn_unsupported("--pack-dyn-relocs=android")?;
    }

    if args.nmagic {
        // GNU_RELRO requires segments to start on page boundaries and cover an entire page
        args.relro = false;
        if args.max_page_size.is_some() {
            args.warning("-z max-page-size is incompatible with --nmagic");
        }
        args.common_mut()
            .inputs
            .iter_mut()
            .for_each(|input| input.modifiers.allow_shared = false);
    }

    if !args.experimental_sframe {
        args.discard_sframe = true;
    }

    Ok(())
}

impl crate::args::HasCommonArgs for ElfArgs {
    fn common(&self) -> &crate::args::CommonArgs {
        &self.common
    }

    fn common_mut(&mut self) -> &mut crate::args::CommonArgs {
        &mut self.common
    }
}

impl platform::Args for ElfArgs {
    fn parse<S, I>(&mut self, input: I) -> Result
    where
        S: AsRef<str>,
        I: Iterator<Item = S>,
    {
        parse(self, input)
    }

    crate::args::impl_platform_args_from_common!();

    fn gc_stats_output_file(&self) -> Option<&Path> {
        self.write_gc_stats.as_deref()
    }

    fn gc_stats_ignore(&self) -> &[String] {
        &self.gc_stats_ignore
    }

    fn verbose_gc_stats(&self) -> bool {
        self.verbose_gc_stats
    }

    fn rosegment(&self) -> bool {
        self.rosegment
    }

    // TODO: Some linkers like ld and mold cleanup debug symbols when linking with -r. For now, we
    // ignore --strip-all and --strip-debug in partial link mode.
    fn should_strip_debug(&self) -> bool {
        !self.should_output_partial_object() && matches!(self.strip, Strip::All | Strip::Debug)
    }

    fn should_strip_all(&self) -> bool {
        !self.should_output_partial_object() && matches!(self.strip, Strip::All)
    }

    fn should_strip_symbol_named(&self, name: &[u8]) -> bool {
        let Strip::Retain(retain) = &self.strip else {
            return false;
        };
        !retain.contains(name)
    }

    fn force_undefined_symbol_names(&self) -> &[String] {
        &self.undefined
    }

    fn lib_search_path(&self) -> &[Box<Path>] {
        &self.lib_search_path
    }

    fn sysroot(&self) -> Option<&Path> {
        self.sysroot.as_deref()
    }

    fn should_gc_sections(&self) -> bool {
        self.gc_sections && !self.common.incremental
    }

    fn orphan_handling(&self) -> crate::platform::OrphanHandling {
        self.orphan_handling
    }

    fn should_merge_sections(&self) -> bool {
        self.merge_sections
    }

    fn force_export_symbol_names(&self) -> &[String] {
        &self.export_list
    }

    fn symbol_names_to_wrap(&self) -> &[String] {
        &self.wrap
    }

    fn entry_point<'a>(
        &'a self,
        linker_script_entry: Option<&'a [u8]>,
    ) -> platform::EntryPoint<'a> {
        // The --entry flag is used first, falling back to what the linker script says, or otherwise
        // defaults to `_start`.
        if let Some(entry) = self.entry.as_deref() {
            return parse_number(entry).map_or_else(
                |_| platform::EntryPoint::Symbol(entry.as_bytes()),
                platform::EntryPoint::Address,
            );
        }
        platform::EntryPoint::Symbol(linker_script_entry.unwrap_or(b"_start"))
    }

    fn start_address_for_section(&self, section_name: SectionName) -> Option<u64> {
        // --section-start takes precedence over -Ttext/-Tdata/-Tbss.
        if let Some(&addr) = self.section_start.get(section_name.bytes()) {
            return Some(addr);
        }
        match section_name.bytes() {
            b".text" => self.ttext,
            b".data" => self.tdata,
            b".bss" => self.tbss,
            _ => None,
        }
    }

    fn segment_start_override(&self, name: crate::parsing::SegmentName) -> Option<u64> {
        match name {
            crate::parsing::SegmentName::Text => self.ttext,
            crate::parsing::SegmentName::Data => self.tdata,
            crate::parsing::SegmentName::Bss => self.tbss,
            crate::parsing::SegmentName::Rodata | crate::parsing::SegmentName::Other => None,
        }
    }

    fn version_script_path(&self) -> Option<&Path> {
        self.version_script_path.as_deref()
    }

    fn export_list_path(&self) -> Option<&Path> {
        self.export_list_path.as_deref()
    }

    fn defsym(&self) -> &[(String, String)] {
        &self.defsym
    }

    fn should_relax(&self) -> bool {
        self.relax
    }

    fn sort_sections_by_name(&self) -> bool {
        self.sort_section == Some(SortSectionMode::Name)
    }

    fn should_emit_got_plt_syms(&self) -> bool {
        self.got_plt_syms
    }

    fn copy_relocations_enabled(&self) -> crate::args::CopyRelocations {
        self.copy_relocations
    }

    fn should_error_on_unresolved_symbols(&self) -> bool {
        self.error_unresolved_symbols
    }

    fn should_write_linker_identity(&self) -> bool {
        self.should_write_linker_identity
    }

    fn dynamic_linker(&self) -> Option<&Path> {
        match &self.dynamic_linker {
            DynamicLinker::EmulationDefault => self.emulation.default_dynamic_linker(),
            DynamicLinker::Omit => None,
            DynamicLinker::Explicit(path) => Some(path),
        }
    }

    fn should_allow_object_undefined(&self, output_kind: OutputKind) -> bool {
        !self.no_undefined.unwrap_or(output_kind.is_executable())
    }

    fn allow_multiple_definitions(&self) -> bool {
        self.allow_multiple_definitions
    }

    fn stack_size_override(&self) -> Option<NonZeroU64> {
        self.z_stack_size
    }

    fn unresolved_symbols_behaviour(&self) -> crate::args::UnresolvedSymbols {
        self.unresolved_symbols
    }

    fn is_ignored_flag(&self, flag: &str) -> bool {
        IGNORED_FLAGS.contains(&flag)
    }

    fn should_export_all_dynamic_symbols(&self) -> bool {
        self.export_all_dynamic_symbols
    }

    fn should_export_dynamic(&self, lib_name: &[u8]) -> bool {
        !self.exclude_libs.should_exclude(lib_name)
    }

    fn loadable_segment_alignment(&self) -> Alignment {
        if self.nmagic {
            return Alignment { exponent: 0 };
        }

        if let Some(max_page_size) = self.max_page_size {
            return max_page_size;
        }

        match self.architecture() {
            Architecture::X86_64 => Alignment { exponent: 12 },
            Architecture::AArch64 => Alignment { exponent: 16 },
            Architecture::RiscV64 => Alignment { exponent: 12 },
            Architecture::LoongArch64 => Alignment { exponent: 16 },
            Architecture::Ppc64 => Alignment { exponent: 16 },
            Architecture::Unsupported => unreachable!(),
        }
    }

    fn common_page_size(&self) -> u64 {
        let max = self.loadable_segment_alignment().value();
        let common = self
            .common_page_size
            .map(Alignment::value)
            .unwrap_or(0x1000);
        common.min(max)
    }

    fn relro(&self) -> bool {
        self.relro
    }

    fn dependency_file(&self) -> Option<&Path> {
        self.dependency_file.as_deref()
    }

    fn should_write_trace_file(&self) -> bool {
        self.trace
    }

    fn should_output_executable(&self) -> bool {
        self.should_output_executable
    }

    fn should_output_partial_object(&self) -> bool {
        self.should_output_partial_object
    }

    fn emit_relocs(&self) -> bool {
        self.emit_relocs
    }

    fn discard_none(&self) -> bool {
        self.discard_none
    }

    fn should_write_gdb_index(&self) -> bool {
        self.gdb_index && !self.should_strip_debug()
    }

    fn architecture(&self) -> Architecture {
        ElfArgs::architecture(self)
    }

    fn output_format_endian(&self) -> Option<Endianness> {
        self.output_format_endian
    }
}

#[cfg(test)]
mod tests {
    use super::ElfArgs;
    use super::parser::SILENTLY_IGNORED_FLAGS;
    use crate::args::InputSpec;
    use crate::args::VersionMode;
    use crate::platform::Args as _;
    use itertools::Itertools;
    use std::fs::File;
    use std::io::BufWriter;
    use std::io::Write;
    use std::num::NonZeroUsize;
    use std::path::Path;
    use std::path::PathBuf;
    use std::str::FromStr;
    use tempfile::NamedTempFile;

    const INPUT1: &[&str] = &[
        "-pie",
        "-z",
        "relro",
        "-zrelro",
        "-hash-style=gnu",
        "--hash-style=gnu",
        "-build-id",
        "--build-id",
        "--eh-frame-hdr",
        "-m",
        "elf_x86_64",
        "-dynamic-linker",
        "/lib64/ld-linux-x86-64.so.2",
        "-o/tmp/a.out",
        "-o",
        "/build/target/debug/deps/c1-a212b73b12b6d123",
        "/lib/x86_64-linux-gnu/Scrt1.o",
        "/lib/x86_64-linux-gnu/crti.o",
        "/usr/bin/../lib/gcc/x86_64-linux-gnu/12/crtbeginS.o",
        "-L/build/target/debug/deps",
        "-L/tool/lib/rustlib/x86_64/lib",
        "-L/tool/lib/rustlib/x86_64/lib",
        "-L/usr/bin/../lib/gcc/x86_64-linux-gnu/12",
        "-L/usr/bin/../lib/gcc/x86_64-linux-gnu/12/../../../../lib64",
        "-L/lib/x86_64-linux-gnu",
        "-L/lib/../lib64",
        "-L/usr/lib/x86_64-linux-gnu",
        "-L/usr/lib/../lib64",
        "-L",
        "/lib",
        "-L/usr/lib",
        "/tmp/rustcDcR20O/symbols.o",
        "/build/target/debug/deps/c1-a212b73b12b6d123.1.rcgu.o",
        "/build/target/debug/deps/c1-a212b73b12b6d123.2.rcgu.o",
        "/build/target/debug/deps/c1-a212b73b12b6d123.3.rcgu.o",
        "/build/target/debug/deps/c1-a212b73b12b6d123.4.rcgu.o",
        "/build/target/debug/deps/c1-a212b73b12b6d123.5.rcgu.o",
        "/build/target/debug/deps/c1-a212b73b12b6d123.6.rcgu.o",
        "/build/target/debug/deps/c1-a212b73b12b6d123.7.rcgu.o",
        "--as-needed",
        "-as-needed",
        "-Bstatic",
        "/tool/lib/rustlib/x86_64/lib/libstd-6498d8891e016dca.rlib",
        "/tool/lib/rustlib/x86_64/lib/libpanic_unwind-3debdee1a9058d84.rlib",
        "/tool/lib/rustlib/x86_64/lib/libobject-8339c5bd5cbc92bf.rlib",
        "/tool/lib/rustlib/x86_64/lib/libmemchr-160ebcebb54c11ba.rlib",
        "/tool/lib/rustlib/x86_64/lib/libaddr2line-95c75789f1b65e37.rlib",
        "/tool/lib/rustlib/x86_64/lib/libgimli-7e8094f2d6258832.rlib",
        "/tool/lib/rustlib/x86_64/lib/librustc_demangle-bac9783ef1b45db0.rlib",
        "/tool/lib/rustlib/x86_64/lib/libstd_detect-a1cd87df2f2d8e76.rlib",
        "/tool/lib/rustlib/x86_64/lib/libhashbrown-7fd06d468d7dba16.rlib",
        "/tool/lib/rustlib/x86_64/lib/librustc_std_workspace_alloc-5ac19487656e05bf.rlib",
        "/tool/lib/rustlib/x86_64/lib/libminiz_oxide-c7c35d32cf825c11.rlib",
        "/tool/lib/rustlib/x86_64/lib/libadler-c523f1571362e70b.rlib",
        "/tool/lib/rustlib/x86_64/lib/libunwind-85f17c92b770a911.rlib",
        "/tool/lib/rustlib/x86_64/lib/libcfg_if-598d3ba148dadcea.rlib",
        "/tool/lib/rustlib/x86_64/lib/liblibc-a58ec2dab545caa4.rlib",
        "/tool/lib/rustlib/x86_64/lib/liballoc-f9dda8cca149f0fc.rlib",
        "/tool/lib/rustlib/x86_64/lib/librustc_std_workspace_core-7ba4c315dd7a3503.rlib",
        "/tool/lib/rustlib/x86_64/lib/libcore-5ac2993e19124966.rlib",
        "/tool/lib/rustlib/x86_64/lib/libcompiler_builtins-df2fb7f50dec519a.rlib",
        "-Bdynamic",
        "-lgcc_s",
        "-lutil",
        "-lrt",
        "-lpthread",
        "-lm",
        "-ldl",
        "-lc",
        "--eh-frame-hdr",
        "-z",
        "noexecstack",
        "-znoexecstack",
        "--gc-sections",
        "-z",
        "relro",
        "-z",
        "now",
        "-z",
        "lazy",
        "-soname=fpp",
        "-soname",
        "bar",
        "/usr/bin/../lib/gcc/x86_64-linux-gnu/12/crtendS.o",
        "/lib/x86_64-linux-gnu/crtn.o",
        "--version-script",
        "a.ver",
        "--no-threads",
        "--no-add-needed",
        "--no-copy-dt-needed-entries",
        "--discard-locals",
        "--use-android-relr-tags",
        "--pack-dyn-relocs=relr",
        "-X",
        "-EL",
        "-O",
        "1",
        "-O3",
        "-v",
        "--sysroot=/usr/aarch64-linux-gnu",
        "--demangle",
        "--no-demangle",
        "-l:lib85caec4suo0pxg06jm2ma7b0o.so",
        "-rpath",
        "foo/",
        "-rpath=bar/",
        "-Rbaz",
        "-R",
        "somewhere",
        // Adding the same rpath multiple times should not create duplicates
        "-rpath",
        "foo/",
        "-x",
        "--discard-all",
        "--dependency-file=deps.d",
        "--sort-section=alignment",
        "--orphan-handling=error",
    ];

    const FILE_OPTIONS: &[&str] = &["-pie"];

    const INLINE_OPTIONS: &[&str] = &["-L", "/lib"];

    fn write_options_to_file(file: &File, options: &[&str]) {
        let mut writer = BufWriter::new(file);
        for option in options {
            writeln!(writer, "{option}").expect("Failed to write to temporary file");
        }
    }

    #[track_caller]
    fn assert_contains(c: &[Box<Path>], v: &str) {
        assert!(c.iter().any(|p| p.as_ref() == Path::new(v)));
    }

    fn input1_assertions(args: &ElfArgs) {
        assert_eq!(
            args.common
                .inputs
                .iter()
                .filter_map(|i| match &i.spec {
                    InputSpec::File(_) | InputSpec::Search(_) => None,
                    InputSpec::Lib(lib_name) => Some(lib_name.as_ref()),
                })
                .collect_vec(),
            &["gcc_s", "util", "rt", "pthread", "m", "dl", "c"]
        );
        assert_contains(&args.lib_search_path, "/lib");
        assert_contains(&args.lib_search_path, "/usr/lib");
        assert!(!args.common.inputs.iter().any(|i| match &i.spec {
            InputSpec::File(f) => f.as_ref() == Path::new("/usr/bin/ld"),
            InputSpec::Lib(_) | InputSpec::Search(_) => false,
        }));
        assert_eq!(
            args.version_script_path,
            Some(PathBuf::from_str("a.ver").unwrap())
        );
        assert_eq!(args.soname, Some("bar".to_owned()));
        assert_eq!(args.common.num_threads, Some(NonZeroUsize::new(1).unwrap()));
        assert_eq!(args.common.version_mode, VersionMode::Verbose);
        assert_eq!(
            args.sysroot,
            Some(Box::from(Path::new("/usr/aarch64-linux-gnu")))
        );
        assert!(args.common.inputs.iter().any(|i| match &i.spec {
            InputSpec::File(_) | InputSpec::Lib(_) => false,
            InputSpec::Search(lib) => lib.as_ref() == "lib85caec4suo0pxg06jm2ma7b0o.so",
        }));
        assert_eq!(args.rpath.as_deref(), Some("foo/:bar/:baz:somewhere"));
        assert_eq!(
            args.dependency_file,
            Some(PathBuf::from_str("deps.d").unwrap())
        );
    }

    fn inline_and_file_options_assertions(args: &ElfArgs) {
        assert_contains(&args.lib_search_path, "/lib");
    }

    #[test]
    fn test_parse_inline_only_options() {
        let mut args = ElfArgs::new().unwrap();
        args.parse(INPUT1.iter()).unwrap();
        input1_assertions(&args);
    }

    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    fn test_parse_file_only_options() {
        // Create a temporary file containing the same options (one per line) as INPUT1
        let file = NamedTempFile::new().expect("Could not create temp file");
        write_options_to_file(file.as_file(), INPUT1);

        // pass the name of the file where options are as the only inline option "@filename"
        let inline_options = [format!("@{}", file.path().to_str().unwrap())];
        let mut args = ElfArgs::new().unwrap();
        args.parse(inline_options.iter()).unwrap();
        input1_assertions(&args);
    }

    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    fn test_parse_mixed_file_and_inline_options() {
        // Create a temporary file containing some options
        let file = NamedTempFile::new().expect("Could not create temp file");
        write_options_to_file(file.as_file(), FILE_OPTIONS);

        // create an inline option referring to "@filename"
        let file_option = format!("@{}", file.path().to_str().unwrap());
        // start with the set of inline options
        let mut inline_options = INLINE_OPTIONS.to_vec();
        // and extend with the "@filename" option
        inline_options.push(&file_option);

        // confirm that this works and the resulting set of options is correct
        let mut args = ElfArgs::new().unwrap();
        args.parse(inline_options.iter()).unwrap();
        inline_and_file_options_assertions(&args);
    }

    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    fn test_parse_overlapping_file_and_inline_options() {
        // Create a set of file options that has a duplicate of an inline option
        let mut file_options = FILE_OPTIONS.to_vec();
        file_options.append(&mut INLINE_OPTIONS.to_vec());
        // and save them to a file
        let file = NamedTempFile::new().expect("Could not create temp file");
        write_options_to_file(file.as_file(), &file_options);

        // pass the name of the file where options are, as an inline option "@filename"
        let file_option = format!("@{}", file.path().to_str().unwrap());
        // start with the set of inline options
        let mut inline_options = INLINE_OPTIONS.to_vec();
        // and extend with the "@filename" option
        inline_options.push(&file_option);

        // confirm that this works and the resulting set of options is correct
        let mut args = ElfArgs::new().unwrap();
        args.parse(inline_options.iter()).unwrap();
        inline_and_file_options_assertions(&args);
    }

    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "wasi doesn't have a temp dir")]
    fn test_parse_recursive_file_option() {
        // Create a temporary file containing a @file option
        let file1 = NamedTempFile::new().expect("Could not create temp file");
        let file2 = NamedTempFile::new().expect("Could not create temp file");
        let file_option = format!("@{}", file2.path().to_str().unwrap());
        write_options_to_file(file1.as_file(), &[&file_option]);
        write_options_to_file(file2.as_file(), INPUT1);

        // pass the name of the file where options are, as an inline option "@filename"
        let inline_options = [format!("@{}", file1.path().to_str().unwrap())];

        // confirm that this works and the resulting set of options is correct
        let mut args = ElfArgs::new().unwrap();
        args.parse(inline_options.iter())
            .expect("Recursive @file options should parse correctly but be ignored");
        input1_assertions(&args);
    }

    #[test]
    fn test_arguments_from_string() {
        use crate::args::arguments_from_string;

        assert_eq!(arguments_from_string("").unwrap(), Vec::<String>::new());
        assert_eq!(arguments_from_string("''").unwrap(), Vec::<String>::new());
        assert_eq!(arguments_from_string("\"\"").unwrap(), Vec::<String>::new());
        assert_eq!(
            arguments_from_string(r#""foo" "bar""#).unwrap(),
            ["foo", "bar"]
        );
        assert_eq!(
            arguments_from_string(r#""foo\"" "\"b\"ar""#).unwrap(),
            ["foo\"", "\"b\"ar"]
        );
        assert_eq!(
            arguments_from_string("   foo  bar      ").unwrap(),
            ["foo", "bar"]
        );
        assert!(arguments_from_string("'foo''bar'").is_err());
        assert_eq!(
            arguments_from_string("'foo' 'bar' baz").unwrap(),
            ["foo", "bar", "baz"]
        );
        assert_eq!(arguments_from_string("foo\nbar").unwrap(), ["foo", "bar"]);
        assert_eq!(
            arguments_from_string(r#"'foo' "bar" baz"#).unwrap(),
            ["foo", "bar", "baz"]
        );
        assert_eq!(arguments_from_string("'foo bar'").unwrap(), ["foo bar"]);
        assert_eq!(
            arguments_from_string("'foo \"  bar'").unwrap(),
            ["foo \"  bar"]
        );
        assert!(arguments_from_string("foo\\").is_err());
        assert!(arguments_from_string("'foo").is_err());
        assert!(arguments_from_string("foo\"").is_err());
    }

    #[test]
    fn test_ignored_flags() {
        for flag in SILENTLY_IGNORED_FLAGS {
            assert!(!flag.starts_with('-'));
        }
    }

    // Helper: parse a small set of args and return the resulting ElfArgs.
    fn parse_args<'a>(args: impl IntoIterator<Item = &'a str>) -> ElfArgs {
        let mut elf_args = ElfArgs::new().unwrap();
        elf_args.parse(args.into_iter()).unwrap();
        elf_args
    }

    // Helper: parse args and expect a parse error.
    fn parse_args_err<'a>(args: impl IntoIterator<Item = &'a str>) -> crate::error::Error {
        let mut elf_args = ElfArgs::new().unwrap();
        elf_args.parse(args.into_iter()).unwrap_err()
    }

    #[test]
    fn test_ttext_hex_round_trip() {
        use crate::output_section_id::SectionName;
        let args = parse_args(["-Ttext=0x700000"]);
        assert_eq!(
            args.start_address_for_section(SectionName(b".text")),
            Some(0x700000)
        );
    }

    #[test]
    fn test_ttext_decimal_round_trip() {
        use crate::output_section_id::SectionName;
        // 7340032 == 0x700000
        let args = parse_args(["-Ttext=7340032"]);
        assert_eq!(
            args.start_address_for_section(SectionName(b".text")),
            Some(0x700000)
        );
    }

    #[test]
    fn test_tdata_hex_round_trip() {
        use crate::output_section_id::SectionName;
        let args = parse_args(["-Tdata=0x800000"]);
        assert_eq!(
            args.start_address_for_section(SectionName(b".data")),
            Some(0x800000)
        );
    }

    #[test]
    fn test_tdata_decimal_round_trip() {
        use crate::output_section_id::SectionName;
        // 8388608 == 0x800000
        let args = parse_args(["-Tdata=8388608"]);
        assert_eq!(
            args.start_address_for_section(SectionName(b".data")),
            Some(0x800000)
        );
    }

    #[test]
    fn test_tbss_hex_round_trip() {
        use crate::output_section_id::SectionName;
        let args = parse_args(["-Tbss=0x900000"]);
        assert_eq!(
            args.start_address_for_section(SectionName(b".bss")),
            Some(0x900000)
        );
    }

    #[test]
    fn test_tbss_decimal_round_trip() {
        use crate::output_section_id::SectionName;
        // 9437184 == 0x900000
        let args = parse_args(["-Tbss=9437184"]);
        assert_eq!(
            args.start_address_for_section(SectionName(b".bss")),
            Some(0x900000)
        );
    }

    #[test]
    fn test_ttext_invalid_address() {
        // Parsing a non-numeric address should return an error.
        parse_args_err(["-Ttext=notanumber"]);
    }

    #[test]
    fn test_section_start_takes_precedence_over_ttext() {
        use crate::output_section_id::SectionName;
        // --section-start=.text=0x600000 should win over -Ttext=0x700000
        let args = parse_args(["--section-start=.text=0x600000", "-Ttext=0x700000"]);
        assert_eq!(
            args.start_address_for_section(SectionName(b".text")),
            Some(0x600000)
        );
    }

    #[test]
    fn test_version_message_matches_gnu_ld_probes() {
        use crate::args::CommonArgs;
        let args = ElfArgs::new().unwrap();
        let msg = args.common.version_message();
        let mut lines = msg.lines();
        let first = lines.next().expect("GNU ld line");
        let identity = lines.next().expect("Wild identity line");
        assert_eq!(
            first,
            format!("GNU ld (Wild) {}", CommonArgs::GNU_LD_COMPAT_VERSION)
        );
        assert!(
            identity.starts_with("Wild "),
            "identity line should name Wild: {identity}"
        );
        assert!(identity.contains("compatible with GNU linkers"));
        // Glibc's sed matches `GNU ld` on a line; the identity line must not.
        assert!(
            !identity.contains("GNU ld"),
            "identity line must not contain `GNU ld` or glibc configure captures the Wild version"
        );
    }

    #[test]
    fn test_gcc15_gnu_ld_flags_parse() {
        parse_args([
            "--no-error-execstack",
            "--warn-execstack",
            "-z",
            "start-stop-gc",
            "-z",
            "nomark-plt",
            "-z",
            "pack-relative-relocs",
        ]);
    }
}
