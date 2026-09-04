use crate::OutputKind;
use crate::Result;
use crate::alignment::Alignment;
use crate::arch::Architecture;
use crate::bail;
use crate::env;
use crate::error::Warning;
use crate::output_section_id::SectionName;
use object::Endianness;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryPoint<'a> {
    None,
    Symbol(&'a [u8]),
    Address(u64),
}

/// GNU `--orphan-handling`. An orphan is an input section not mentioned in the
/// linker script (or built-in rules). Placement among neighbouring output
/// sections still follows Wild's custom-section path, not GNU's insertion
/// heuristic.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum OrphanHandling {
    /// Place the section in a same-named output section (default).
    #[default]
    Place,
    /// Place the section and emit a warning.
    Warn,
    /// Fail the link if any orphan is found.
    Error,
    /// Drop the section, as if it were matched by `/DISCARD/`.
    Discard,
}

pub(crate) trait Args: std::fmt::Debug + Send + Sync + 'static {
    fn parse<S, I>(&mut self, input: I) -> Result
    where
        S: AsRef<str>,
        I: Iterator<Item = S>;

    fn gc_stats_output_file(&self) -> Option<&Path> {
        None
    }

    fn gc_stats_ignore(&self) -> &[String] {
        &[]
    }

    fn verbose_gc_stats(&self) -> bool {
        false
    }

    fn should_strip_debug(&self) -> bool;

    fn should_strip_all(&self) -> bool;

    /// Returns whether a symbol with the specified name should be stripped. Should return false if
    /// name-based stripping is not being applied.
    fn should_strip_symbol_named(&self, _name: &[u8]) -> bool {
        false
    }

    /// Returns a list of symbol names that should be treated as undefined.
    fn force_undefined_symbol_names(&self) -> &[String] {
        &[]
    }

    fn force_export_symbol_names(&self) -> &[String] {
        &[]
    }

    fn symbol_names_to_wrap(&self) -> &[String] {
        &[]
    }

    fn entry_point<'a>(&'a self, linker_script_entry: Option<&'a [u8]>) -> EntryPoint<'a>;

    fn version_script_path(&self) -> Option<&Path> {
        None
    }

    fn lib_search_path(&self) -> &[Box<Path>];

    fn output(&self) -> &Arc<Path> {
        &self.common().output
    }

    fn common(&self) -> &crate::args::CommonArgs;

    fn common_mut(&mut self) -> &mut crate::args::CommonArgs;

    fn sysroot(&self) -> Option<&Path> {
        None
    }

    fn export_list_path(&self) -> Option<&Path> {
        None
    }

    fn should_gc_sections(&self) -> bool {
        true
    }

    fn orphan_handling(&self) -> OrphanHandling {
        OrphanHandling::Place
    }

    fn should_relax(&self) -> bool {
        false
    }

    fn sort_sections_by_name(&self) -> bool {
        false
    }

    fn rosegment(&self) -> bool {
        true
    }

    fn should_emit_got_plt_syms(&self) -> bool {
        false
    }

    fn should_export_all_dynamic_symbols(&self) -> bool;

    /// Returns whether all symbols from the specified input should be exported as dynamic symbols.
    fn should_export_dynamic(&self, lib_name: &[u8]) -> bool;

    /// Returns whether to allow undefined symbols in regular object files.
    fn should_allow_object_undefined(&self, _output_kind: OutputKind) -> bool {
        false
    }

    /// Returns whether multiple symbols with the same name should be permitted.
    fn allow_multiple_definitions(&self) -> bool {
        false
    }

    fn unresolved_symbols_behaviour(&self) -> crate::args::UnresolvedSymbols {
        crate::args::UnresolvedSymbols::ReportAll
    }

    fn defsym(&self) -> &[(String, String)] {
        &[]
    }

    fn stack_size_override(&self) -> Option<NonZeroU64> {
        None
    }

    fn copy_relocations_enabled(&self) -> crate::args::CopyRelocations {
        crate::args::CopyRelocations::Disallowed(
            crate::args::CopyRelocationsDisabledReason::Unsupported,
        )
    }

    fn should_error_on_unresolved_symbols(&self) -> bool {
        true
    }

    /// Whether the linker name and version should be written into the output file.
    fn should_write_linker_identity(&self) -> bool {
        false
    }

    fn dynamic_linker(&self) -> Option<&Path> {
        None
    }

    /// Gives the command-line the option to force the start address for a section based on its
    /// name.
    fn start_address_for_section(&self, _section_name: SectionName) -> Option<u64> {
        None
    }

    /// Returns the address override for a `SEGMENT_START` segment name, as set via
    /// `-Ttext`, `-Tdata` or `-Tbss` on the command line. Returns `None` if no override
    /// was provided, in which case `SEGMENT_START` should return its default value.
    fn segment_start_override(&self, _name: crate::parsing::SegmentName) -> Option<u64> {
        None
    }

    fn loadable_segment_alignment(&self) -> Alignment;

    /// `CONSTANT(COMMONPAGESIZE)` / `-z common-page-size`. Not larger than the max page size.
    fn common_page_size(&self) -> u64 {
        0x1000.min(self.loadable_segment_alignment().value())
    }

    /// `-z relro` (default on). Controls `DATA_SEGMENT_RELRO_END` padding.
    fn relro(&self) -> bool {
        true
    }

    fn should_merge_sections(&self) -> bool;

    fn dependency_file(&self) -> Option<&Path> {
        None
    }

    fn should_write_trace_file(&self) -> bool {
        false
    }

    fn relocation_model(&self) -> crate::args::RelocationModel {
        self.common().relocation_model
    }

    fn should_write_gdb_index(&self) -> bool {
        false
    }

    fn should_output_executable(&self) -> bool;

    fn is_ignored_flag(&self, _flag: &str) -> bool;

    fn warning(&self, message: impl Into<String>) {
        (self.common().warning_callback)(Warning::new(message.into()));
    }

    fn warn_unsupported(&self, opt: &str) -> Result {
        use crate::args::WILD_UNSUPPORTED_ENV;

        let message = format!("{opt} is not yet supported");

        match env::var(WILD_UNSUPPORTED_ENV).unwrap_or_default().as_str() {
            "warn" | "" => self.warning(message),
            "ignore" => {}
            "error" => bail!("{message}"),
            other => bail!("Unsupported value for {WILD_UNSUPPORTED_ENV}={other}"),
        }
        Ok(())
    }

    fn should_output_partial_object(&self) -> bool {
        false
    }

    /// `--emit-relocs` / `-q`: copy input relocation records into the fully linked output.
    fn emit_relocs(&self) -> bool {
        false
    }

    /// `--discard-none`: keep local symbols that would otherwise be omitted (`.L`, mapping
    /// symbols).
    fn discard_none(&self) -> bool {
        false
    }

    /// Copy input `SHT_REL`/`SHT_RELA` contents into the output (`-r` or `--emit-relocs`).
    fn should_copy_input_relocs(&self) -> bool {
        self.should_output_partial_object() || self.emit_relocs()
    }

    fn architecture(&self) -> Architecture {
        Architecture::Unsupported
    }

    fn output_format_endian(&self) -> Option<Endianness> {
        None
    }
}
