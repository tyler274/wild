use super::format::Platform;
use super::object::SourceInfo;
use crate::OutputKind;
use crate::Result;
use crate::bail;
use crate::layout::Layout;
use crate::part_id::PartId;
use crate::value_flags::ValueFlags;
use linker_utils::elf::DynamicRelocationKind;
use linker_utils::elf::RelocationKindInfo;
use linker_utils::relaxation::RelocationModifier;
use linker_utils::relaxation::SectionRelaxDeltas;
use std::borrow::Cow;

/// Configuration for range-extension thunks on architectures that need them.
/// Returned by `Arch::thunk_config()`; `None` means the architecture never needs thunks.
pub(crate) struct ThunkConfig {
    /// PartId for the primary function part (main `.text` alignment bucket). This is the
    /// alignment used by the vast majority of code and is where per-object thunks are placed.
    pub(crate) primary_function_part_id: PartId,

    /// Minimum branch range across all range-limited branch relocations for this architecture.
    /// If the total executable input size is below this, thunks can be disabled entirely.
    pub(crate) min_branch_range: u64,

    /// Size in bytes of a single thunk. Must be a multiple of the `primary_function_part_id`
    /// alignment.
    pub(crate) thunk_size: u64,
}

/// Represents a supported architecture. Note that implementations are file-format specific.
pub(crate) trait Arch: Send + Sync + 'static {
    type Relaxation: Relaxation;
    type Platform: Platform;

    /// Default load address for non-PIE output.
    /// Override this for architectures that need a different default.
    const DEFAULT_LOAD_ADDRESS: u64 = 0x400_000;

    /// Number of entries reserved by the runtime at the start of the table addressed by DT_PLTGOT.
    const NUM_GOT_PLT_HEADER_ENTRIES: u64 = 0;

    /// Returns the identifier to be written into the output file that identifies the file as
    /// belonging to this architecture. e.g. for ELF, this is the header magic for the architecture.
    fn arch_identifier() -> <Self::Platform as Platform>::ArchIdentifier;

    /// Get dynamic relocation value specific for the architecture.
    fn get_dynamic_relocation_type(
        relocation: DynamicRelocationKind,
    ) -> <Self::Platform as Platform>::RelocationInfo;

    /// Write PLT entry for the architecture.
    fn write_plt_entry(plt_entry: &mut [u8], got_address: u64, plt_address: u64) -> Result;

    /// Make architecture-specific parsing of the relocation types.
    fn relocation_from_raw(
        r_type: <Self::Platform as Platform>::RelocationInfo,
    ) -> Result<RelocationKindInfo>;

    /// Get string representation of a relocation specific for the architecture.
    fn rel_type_to_string(
        r_type: <Self::Platform as Platform>::RelocationInfo,
    ) -> Cow<'static, str>;

    /// Get DTV OFFSET.
    fn get_dtv_offset() -> u64 {
        0
    }

    /// Get position of the $tp (thread pointer) in the TLS section. Each platform defines
    /// a different place based on the following article:
    /// https://maskray.me/blog/2021-02-14-all-about-thread-local-storage#tls-variants
    fn tp_offset_start(layout: &Layout<Self::Platform>) -> u64;

    /// Classify a GNU property note.
    fn get_property_class(property_type: u32) -> Option<crate::elf::PropertyClass>;

    /// Merge e_flags of the input files and provide an error
    /// if the flags are not compatible.
    fn merge_eflags(
        eflags: impl Iterator<Item = <Self::Platform as Platform>::FileFlags>,
    ) -> Result<<Self::Platform as Platform>::FileFlags>;

    /// A list of high-part relocations that need to be tracked in a relocation cache
    fn high_part_relocations() -> &'static [<Self::Platform as Platform>::RelocationInfo];

    /// Whether the platform supports relaxations that reduce the sizes of function.
    fn supports_size_reduction_relaxations() -> bool {
        false
    }

    /// Returns true if the given relocation type cannot be used against interposable symbols.
    /// This includes preemptible symbols in shared objects and DSO-provided symbols in
    /// executables. On 64-bit architectures, sub-pointer-size absolute and PC-relative
    /// relocations cannot hold runtime-resolved addresses. Default is false (allow).
    fn is_disallowed_for_interposable_symbols(
        _r_type: <Self::Platform as Platform>::RelocationInfo,
    ) -> bool {
        false
    }

    /// Returns true if the given relocation type cannot be used when making a shared object,
    /// regardless of whether the symbol is interposable. Default is false (allow).
    fn is_disallowed_in_shared_object(
        _r_type: <Self::Platform as Platform>::RelocationInfo,
    ) -> bool {
        false
    }

    /// Uses debug info, if available, to get information about where in the source code a
    /// particular offset in a particular section came from.
    fn get_source_info<'data>(
        object: &<Self::Platform as Platform>::File<'data>,
        relocations: &<Self::Platform as Platform>::RelocationSections,
        section: &<Self::Platform as Platform>::SectionHeader,
        offset_in_section: u64,
    ) -> Result<SourceInfo>;

    fn collect_relaxation_deltas<'data>(
        _section_output_address: u64,
        _section_bytes: &[u8],
        _relocations: <Self::Platform as Platform>::RelocationList<'data>,
        _existing_deltas: Option<&SectionRelaxDeltas>,
        _resolve_symbol: impl FnMut(object::SymbolIndex) -> Option<RelaxSymbolInfo>,
    ) -> (Vec<(u64, u32)>, Option<u64>) {
        // This function should not be called unless `supports_size_reduction_relaxations` returns
        // true in which case this function should be implemented.
        unreachable!();
    }

    fn is_symbol_variant_pcs(
        _object: &<Self::Platform as Platform>::File<'_>,
        _symbol_index: object::SymbolIndex,
    ) -> bool {
        false
    }

    /// For a call/branch relocation, returns the offset from a callee's global entry point to its
    /// local entry point, derived from the callee's `st_other`. Only meaningful on ppc64 (ELFv2
    /// dual-entry functions); defaults to 0.
    fn local_entry_offset(_st_other: u8) -> u64 {
        0
    }

    /// Tries to create a relaxation for the relocation of the specified kind, to be applied at the
    /// specified offset in the supplied section.
    fn new_relaxation(
        relocation_kind: <Self::Platform as Platform>::RelocationInfo,
        section_bytes: &[u8],
        offset_in_section: u64,
        flags: ValueFlags,
        output_kind: OutputKind,
        section_flags: <Self::Platform as Platform>::SectionFlags,
        relax_deltas: Option<&SectionRelaxDeltas>,
        sym_addr: u64,
        section_address: u64,
        rel_addend: i64,
        previous_relocation: Option<
            PreviousRelocationInfo<<Self::Platform as Platform>::RelocationInfo>,
        >,
    ) -> Option<Self::Relaxation>;

    /// Fill `buf` with NOP padding.
    fn fill_nop_padding(_buf: &mut [u8]) {}

    fn process_riscv_attributes<'data>(
        _object: &<Self::Platform as Platform>::File<'data>,
        _format_specific: &mut <Self::Platform as Platform>::ObjectLayoutStateExt<'data>,
        _riscv_attributes_section_index: object::SectionIndex,
    ) -> Result {
        bail!(".riscv.attribute section is supported only for riscv64 target");
    }

    /// Returns the thunk configuration for this architecture, or `None` if this architecture
    /// doesn't need thunks or we just don't support them yet.
    fn thunk_config() -> Option<ThunkConfig> {
        None
    }

    /// Writes a thunk into the supplied buffer that jumps to the given target address. The thunk is
    /// placed at `thunk_address`. The buffer size equals `ThunkConfig::thunk_size`. The thunk must
    /// be position-independent (PC-relative).
    fn write_thunk(_thunk_address: u64, _target_address: u64, _buf: &mut [u8]) {
        // Should only be called if thunk_config returns Some, in which case this must be
        // overridden.
        unimplemented!();
    }

    /// Return the starting load address for non-PIE output.
    fn start_memory_address(output_kind: OutputKind) -> u64 {
        if output_kind.has_fixed_load_address() {
            Self::DEFAULT_LOAD_ADDRESS
        } else {
            0
        }
    }

    fn fill_section_padding(buf: &mut [u8], _section_flags: object::elf::SectionFlags) {
        buf.fill(0);
    }
}

pub(crate) trait Relaxation: Send + Sync + 'static {
    fn apply(&self, section_bytes: &mut [u8], offset_in_section: &mut u64, addend: &mut i64);

    fn rel_info(&self) -> RelocationKindInfo;

    fn debug_kind(&self) -> impl std::fmt::Debug;

    fn next_modifier(&self) -> RelocationModifier;

    fn is_mandatory(&self) -> bool;
}

pub(crate) struct RelaxSymbolInfo {
    /// The symbol's approximate output address (section base + offset within section).
    pub output_address: u64,
    /// Whether the symbol may be interposed at runtime.
    pub is_interposable: bool,
}

/// Information about the previous relocation, used for pair-based relaxations.
#[allow(dead_code)]
pub(crate) struct PreviousRelocationInfo<RelInfo> {
    pub kind: RelInfo,
    pub offset: u64,
    pub symbol: Option<object::SymbolIndex>,
    pub addend: i64,
}
