use super::format::Platform;
use super::output_section_id::OutputSectionId;
use super::part_id::PartId;
use crate::Result;
use crate::input_data::InputBytes;
use crate::input_data::InputRef;
use crate::output_section_part_map::OutputSectionPartMap;
use std::borrow::Cow;
use std::fmt::Display;
use std::num::NonZeroU32;
use std::ops::Range;
use std::path::PathBuf;

/// Symbol visibility. Lives here so `platform/` does not import `symbol_db`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Visibility {
    Default,
    Protected,
    Hidden,
}

/// Abstracts over the different object file formats that we support (or may support). e.g. ELF.
pub(crate) trait ObjectFile<'data>: Sized + Send + Sync + std::fmt::Debug + 'data {
    type Platform: Platform<File<'data> = Self>;

    fn parse_bytes(input: &'data [u8], is_dynamic: bool) -> Result<Self>;

    /// As for `parse_bytes` but also validates that the file architecture matches what is expected
    /// based on `args`.
    fn parse(input: &InputBytes<'data>, args: &<Self::Platform as Platform>::Args) -> Result<Self>;

    fn is_dynamic(&self) -> bool;

    fn num_symbols(&self) -> usize;

    fn enumerate_symbols(
        &self,
    ) -> impl Iterator<
        Item = (
            object::SymbolIndex,
            &<Self::Platform as Platform>::SymtabEntry,
        ),
    > {
        self.symbols_iter()
            .enumerate()
            .map(|(i, sym)| (object::SymbolIndex(i), sym))
    }

    fn symbols_iter(&self) -> impl Iterator<Item = &<Self::Platform as Platform>::SymtabEntry>;

    fn symbol(
        &self,
        index: object::SymbolIndex,
    ) -> Result<&<Self::Platform as Platform>::SymtabEntry>;

    fn section_size(&self, header: &<Self::Platform as Platform>::SectionHeader) -> Result<u64>;

    fn symbol_name(
        &self,
        symbol: &<Self::Platform as Platform>::SymtabEntry,
    ) -> Result<&'data [u8]>;

    // Get the offset of a symbol relative to the section identified by `section_index`.
    fn symbol_offset_in_section(
        &self,
        symbol: &<Self::Platform as Platform>::SymtabEntry,
        section_index: object::SectionIndex,
    ) -> Result<u64>;

    fn num_sections(&self) -> usize;

    fn section_iter<'a>(&'a self) -> <Self::Platform as Platform>::SectionIterator<'a>;

    fn enumerate_sections(
        &self,
    ) -> impl Iterator<
        Item = (
            object::SectionIndex,
            &<Self::Platform as Platform>::SectionHeader,
        ),
    >;

    fn section(
        &self,
        index: object::SectionIndex,
    ) -> Result<&<Self::Platform as Platform>::SectionHeader>;

    fn section_by_name(
        &self,
        name: &str,
    ) -> Option<(
        object::SectionIndex,
        &<Self::Platform as Platform>::SectionHeader,
    )>;

    fn symbol_section(
        &self,
        symbol: &<Self::Platform as Platform>::SymtabEntry,
        index: object::SymbolIndex,
    ) -> Result<Option<object::SectionIndex>>;

    fn symbol_versions(&self) -> &[<Self::Platform as Platform>::SymbolVersionIndex];

    fn dynamic_symbol_used(
        &self,
        _symbol_index: object::SymbolIndex,
        _file: &mut <Self::Platform as Platform>::DynamicLayoutState<'data>,
    ) -> Result {
        unimplemented!();
    }

    fn finalise_sizes_dynamic(
        &self,
        lib_name: &[u8],
        state: &mut <Self::Platform as Platform>::DynamicLayoutStateExt<'data>,
        mem_sizes: &mut OutputSectionPartMap<u64>,
    ) -> Result;

    fn apply_non_addressable_indexes_dynamic(
        &self,
        indexes: &mut <Self::Platform as Platform>::NonAddressableIndexes,
        counts: &mut <Self::Platform as Platform>::NonAddressableCounts,
        state: &mut <Self::Platform as Platform>::DynamicLayoutStateExt<'data>,
    ) -> Result;

    fn section_name(&self, index: object::SectionIndex) -> Result<&'data [u8]>;

    /// Returns the raw section data. Doesn't handle decompression.
    fn raw_section_data(
        &self,
        section: &<Self::Platform as Platform>::SectionHeader,
    ) -> Result<&'data [u8]>;

    fn section_data(
        &self,
        section: &<Self::Platform as Platform>::SectionHeader,
        member: &bumpalo_herd::Member<'data>,
        loaded_metrics: &<Self::Platform as Platform>::LoadedMetrics,
    ) -> Result<&'data [u8]>;

    /// Copies the data for the specified section into `out`, which must be the correct size.
    /// Decompresses the data if necessary.
    fn copy_section_data(
        &self,
        section: &<Self::Platform as Platform>::SectionHeader,
        out: &mut [u8],
    ) -> Result;

    /// Returns the contents of a section as a Cow. Will heap-allocate if the section is compressed.
    fn section_data_cow(
        &self,
        section: &<Self::Platform as Platform>::SectionHeader,
    ) -> Result<Cow<'data, [u8]>>;

    fn section_alignment(
        &self,
        section: &<Self::Platform as Platform>::SectionHeader,
    ) -> Result<u64>;

    fn relocations(
        &self,
        index: object::SectionIndex,
        relocations: &<Self::Platform as Platform>::RelocationSections,
    ) -> Result<<Self::Platform as Platform>::RelocationList<'data>>;

    fn parse_relocations(&self) -> Result<<Self::Platform as Platform>::RelocationSections>;

    /// Whether `index` has an associated relocation section. `SHF_MERGE` inputs with relocs
    /// must not be unique'd: GNU ld concatenates them because reloc fields are often zero in the
    /// file and would otherwise collapse.
    fn section_has_relocations(
        &self,
        _index: object::SectionIndex,
        _relocations: &<Self::Platform as Platform>::RelocationSections,
    ) -> bool {
        false
    }

    /// Get the version of a symbol. Only intended for diagnostic purposes since it's potentially
    /// quite slow.
    fn symbol_version_debug(&self, symbol_index: object::SymbolIndex) -> Option<String>;

    fn section_display_name(&self, index: object::SectionIndex) -> Cow<'data, str>;

    fn dynamic_tag_values(&self) -> Option<<Self::Platform as Platform>::DynamicTagValues<'data>>;

    fn get_version_names(&self) -> Result<<Self::Platform as Platform>::VersionNames<'data>>;

    fn get_symbol_name_and_version(
        &self,
        symbol: &<Self::Platform as Platform>::SymtabEntry,
        local_index: usize,
        version_names: &<Self::Platform as Platform>::VersionNames<'data>,
    ) -> Result<<Self::Platform as Platform>::RawSymbolName<'data>>;

    /// Returns whether we should check for undefined symbols in `self`. Only called for dynamic
    /// objects.
    fn should_enforce_undefined(
        &self,
        resources: &<Self::Platform as Platform>::GraphResources<'data, '_>,
    ) -> bool;

    fn verneed_table(&self) -> Result<<Self::Platform as Platform>::VerneedTable<'data>>;

    fn process_gnu_note_section(
        &self,
        state: &mut <Self::Platform as Platform>::ObjectLayoutStateExt<'data>,
        section_index: object::SectionIndex,
    ) -> Result;

    fn dynamic_tags(&self) -> Result<&'data [<Self::Platform as Platform>::DynamicEntry]>;
}

pub(crate) trait SectionHeader: std::fmt::Debug + Send + Sync + 'static {
    fn is_alloc(&self) -> bool;

    fn is_writable(&self) -> bool;

    fn is_executable(&self) -> bool;

    fn is_tls(&self) -> bool;

    fn is_merge_section(&self) -> bool;

    fn is_strings(&self) -> bool;

    /// `sh_entsize` for `SHF_MERGE` inputs. Zero if the platform has no merge entsize.
    fn merge_entsize(&self) -> u64 {
        0
    }

    fn should_retain(&self) -> bool;

    fn should_exclude(&self) -> bool;

    fn is_group(&self) -> bool;

    fn is_note(&self) -> bool;

    fn is_prog_bits(&self) -> bool;

    /// Returns whether the section has no contents in the file (zero initialised).
    fn is_no_bits(&self) -> bool;

    /// GNU ld does not match these with linker-script wildcards. Input
    /// `SHT_REL`/`SHT_RELA` (`.rela.text`) must not fill `.rela.dyn : { *(.rela.*) }`,
    /// and input `SHT_SYMTAB`/`SHT_STRTAB` must not be concatenated into the
    /// linker's tables via `*(.symtab)` / `*(.strtab)` (kernel `vmlinux.lds`).
    fn skip_linker_script_matching(&self) -> bool {
        false
    }

    /// Input `SHT_REL` / `SHT_RELA` sections copied by `-r` / `--emit-relocs`.
    fn is_reloc_section(&self) -> bool {
        false
    }

    /// GNU `--emit-relocs` names: `.rela` or `.rel` concatenated with the target
    /// output section name (`.text` → `.rela.text`).
    fn reloc_output_name_prefix(&self) -> Option<&'static [u8]> {
        None
    }

    /// `sh_info` of an input `SHT_REL`/`SHT_RELA` section: the relocated section.
    fn reloc_target_section_index(&self) -> Option<object::SectionIndex> {
        None
    }
}

pub(crate) trait SectionType:
    Default + Copy + Send + Sync + std::fmt::Debug + 'static
{
    fn is_rela(&self) -> bool;
    fn is_rel(&self) -> bool;
    fn is_symtab(&self) -> bool;
    fn is_strtab(&self) -> bool;
}

pub(crate) trait SegmentType:
    Default + Copy + Send + Sync + std::fmt::Debug + 'static
{
}

pub(crate) trait SectionFlags:
    Default + Copy + std::fmt::Debug + Send + Sync + 'static
{
    fn is_alloc(self) -> bool;
}

pub(crate) trait Symbol: std::fmt::Debug + Copy + Send + Sync + 'static {
    /// Returns information about the symbol if it's a common symbol. Platforms that don't have
    /// common symbols can just return None.
    fn as_common(&self) -> Option<CommonSymbol>;

    fn is_common(&self) -> bool {
        self.as_common().is_some()
    }

    fn is_undefined(&self) -> bool;

    fn is_local(&self) -> bool;

    fn is_absolute(&self) -> bool;

    fn is_weak(&self) -> bool;

    fn visibility(&self) -> Visibility;

    fn value(&self) -> u64;

    fn size(&self) -> u64;

    fn has_name(&self) -> bool;

    /// Returns whether this symbol should be omitted from the output symtab by default.
    fn is_default_strippable(&self, name: &[u8]) -> bool;

    fn debug_string(&self) -> String;

    /// Returns whether this symbol has been declared as a TLS variable.
    fn is_tls(&self) -> bool;

    /// Returns whether this symbol can be interposed (overridden) at runtime by DSOs earlier in the
    /// load order.
    fn is_interposable(&self) -> bool;

    fn is_func(&self) -> bool;

    fn is_ifunc(&self) -> bool;

    fn is_hidden(&self) -> bool;

    fn is_gnu_unique(&self) -> bool;

    fn with_hidden(self, hidden: bool) -> Self;
}

#[derive(Clone, Copy)]
pub(crate) struct CommonSymbol {
    pub(crate) size: u64,
    pub(crate) part_id: PartId,
}

pub(crate) trait Relocation: Send + Sync + Copy + 'static {
    type Sequence<'data>: RelocationSequence<'data, Rel = Self>;
    type Platform: Platform;

    fn symbol(&self) -> Option<object::SymbolIndex>;

    fn raw_type(&self) -> <Self::Platform as Platform>::RelocationInfo;

    fn offset(&self) -> u64;

    fn addend(&self) -> i64;
}

pub(crate) trait RelocationSequence<'data> {
    type Rel: Relocation;

    fn rel_iter(&self) -> impl Iterator<Item = Self::Rel>;
    fn subsequence(&self, range: Range<usize>) -> Self;
    fn num_relocations(&self) -> usize;
}

pub(crate) trait RelocationList<'data>: Send + Sync + 'data {
    fn num_relocations(&self) -> usize;
}

pub(crate) trait RawSymbolName<'data>: Send + Sync + std::fmt::Display + 'data {
    fn parse(bytes: &'data [u8]) -> Self;

    fn name(&self) -> &'data [u8];

    fn version_name(&self) -> Option<&'data [u8]>;

    fn is_default(&self) -> bool;
}

pub(crate) trait VerneedTable<'data>: Send + Sync + 'data {
    fn version_name(&self, local_symbol_index: object::SymbolIndex) -> Option<&'data [u8]>;
}

pub(crate) trait DynamicTagValues<'data>: std::fmt::Debug + Send + Sync + 'data {
    fn lib_name(&self, input: &InputRef<'data>) -> &'data [u8];
}

pub(crate) trait NonAddressableIndexes: Send + Sync + 'static {
    fn new<P: Platform>(symbol_db: &P::SymbolDb<'_>) -> Self;
}

pub(crate) trait SectionAttributes:
    std::fmt::Debug + Default + Send + Sync + Copy + 'static
{
    type Platform: Platform;

    fn merge(&mut self, rhs: Self);

    fn apply(
        &self,
        output_sections: &mut <Self::Platform as Platform>::OutputSections<'_>,
        section_id: OutputSectionId,
    );

    fn is_null(&self) -> bool;

    fn is_alloc(&self) -> bool;

    fn is_executable(&self) -> bool;

    fn is_tls(&self) -> bool;

    fn occupies_only_tls_address_space(&self) -> bool;

    fn is_writable(&self) -> bool;

    fn is_no_bits(&self) -> bool;

    fn flags(&self) -> <Self::Platform as Platform>::SectionFlags;

    fn ty(&self) -> <Self::Platform as Platform>::SectionType;

    /// Called for custom sections that return true to `is_null`.
    fn set_to_default_type(&mut self);

    /// Mark a custom script section as allocated. Empty kernel-style sections with `AT()` sit
    /// in a `PT_LOAD` and need `SHF_ALLOC` so they contribute to segment bounds.
    fn set_alloc(&mut self) {}

    /// Mark a custom script section as `NOBITS`. Used when the section has no file contents
    /// (only `. +=` reservations and symbol assignments).
    fn set_no_bits(&mut self) {}

    /// Mark a custom script section as writable. GNU ld copies `PF_W` from the
    /// assigned `PT_LOAD` onto script-only sections (kernel `.orc_lookup`).
    fn set_writable(&mut self) {}

    /// True when the script used `(INFO)` / `(DSECT)` / `(COPY)` / `(OVERLAY)` so ALLOC
    /// should not be inferred.
    fn avoids_alloc(&self) -> bool {
        false
    }
}

pub(crate) struct SourceInfo(pub(crate) Option<SourceInfoDetails>);

#[derive(Debug)]
pub(crate) struct SourceInfoDetails {
    pub(crate) path: PathBuf,
    pub(crate) line: u64,
}

/// An index into the exception frames for an object. Interpretation of the value is up to the
/// platform.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameIndex(NonZeroU32);

impl FrameIndex {
    pub(crate) fn from_usize(raw: usize) -> Self {
        Self(NonZeroU32::new(raw as u32 + 1).unwrap())
    }

    pub(crate) fn as_usize(self) -> usize {
        self.0.get() as usize - 1
    }
}

pub(crate) trait ProgramSegmentDef: Copy + Send + Sync + Display + 'static {
    fn is_writable(self) -> bool;

    fn is_executable(self) -> bool;

    fn always_keep(self) -> bool;

    fn is_loadable(self) -> bool;

    fn is_stack(self) -> bool;

    fn is_tls(self) -> bool;

    /// Returns a numeric value that can be used to sort the segments as they should appear in the
    /// program headers table. Segments with lower values will appear first.
    fn order_key(self) -> usize;

    /// Returns whether the current RW segment should end when this segment ends.
    fn should_cut_rw_segment_when_ending(self) -> bool {
        false
    }

    fn from_linker_script(_ptype: u32, _flags: u32) -> Self {
        unreachable!("This function is only called from platforms that support linker scripts.");
    }
}

pub(crate) trait BuiltInSectionDetails: Send + Sync + 'static {}
