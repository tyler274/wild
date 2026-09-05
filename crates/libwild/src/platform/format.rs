use super::cli::Args;
use super::isa::Arch;
use super::isa::ThunkConfig;
use super::object::*;
use crate::FileSystem;
use crate::OutputKind;
use crate::Result;
use crate::alignment::Alignment;
use crate::bail;
use crate::fs::FileReplacementMode;
use crate::layout_rules::SectionRule;
use crate::layout_rules::SectionRuleOutcome;
use crate::linker_script;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionIdentity;
use crate::output_section_id::SectionName;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id;
use crate::part_id::PartId;
use crate::program_segments::ProgramSegments;
use crate::symbol_db::SymbolId;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use rayon::Scope;
use std::num::NonZeroU32;

/// A platform for which we support writing producing linked outputs.
pub(crate) trait Platform:
    Copy + Send + Sync + Sized + Default + std::fmt::Debug + 'static
{
    const NUM_SINGLE_PART_SECTIONS: u32;
    const NUM_BUILT_IN_REGULAR_SECTIONS: usize;

    /// How existing regular output files are replaced when the user doesn't specify a mode.
    const DEFAULT_FILE_REPLACEMENT_MODE: FileReplacementMode =
        FileReplacementMode::UpdateInPlaceWithFallback;

    // TODO: Some of these are very specific to a single platform. Investigate if the code that
    // references them could be moved, allowing the constant to be deleted.

    const TEXT_SECTION_ID: Option<OutputSectionId> = None;
    const DATA_SECTION_ID: Option<OutputSectionId> = None;
    const BSS_SECTION_ID: Option<OutputSectionId> = None;
    const RODATA_SECTION_ID: Option<OutputSectionId> = None;
    const TDATA_SECTION_ID: Option<OutputSectionId> = None;
    const TBSS_SECTION_ID: Option<OutputSectionId> = None;
    const STRTAB_SECTION_ID: Option<OutputSectionId> = None;
    const SYMTAB_LOCAL_SECTION_ID: Option<OutputSectionId> = None;
    const SYMTAB_GLOBAL_SECTION_ID: Option<OutputSectionId> = None;
    const SYMTAB_SHNDX_LOCAL_SECTION_ID: Option<OutputSectionId> = None;
    const SYMTAB_SHNDX_GLOBAL_SECTION_ID: Option<OutputSectionId> = None;
    const GDB_INDEX_SECTION_ID: Option<OutputSectionId> = None;
    const DYNSTR_SECTION_ID: Option<OutputSectionId> = None;
    const DYNSYM_SECTION_ID: Option<OutputSectionId> = None;
    const GOT_SECTION_ID: Option<OutputSectionId> = None;
    const PLT_GOT_SECTION_ID: Option<OutputSectionId> = None;
    const EH_FRAME_SECTION_ID: Option<OutputSectionId> = None;
    const NOTE_GNU_PROPERTY_SECTION_ID: Option<OutputSectionId> = None;
    const NOTE_GNU_BUILD_ID_SECTION_ID: Option<OutputSectionId> = None;
    const RISCV_ATTRIBUTES_SECTION_ID: Option<OutputSectionId> = None;
    const GOT_RELR_SECTION_ID: Option<OutputSectionId> = None;
    const GNU_VERSION_SECTION_ID: Option<OutputSectionId> = None;
    const COMMENT_SECTION_ID: Option<OutputSectionId> = None;
    const INTERP_SECTION_ID: Option<OutputSectionId> = None;
    const SFRAME_SECTION_ID: Option<OutputSectionId> = None;
    const RELRO_PADDING_SECTION_ID: Option<OutputSectionId> = None;

    const CUSTOM_PHDR_EXCLUDED_SECTION_IDS: &'static [OutputSectionId] = &[];
    const PACKED_SECTION_IDS: &'static [OutputSectionId] = &[];
    const VERIFY_IGNORE_SECTION_IDS: &'static [OutputSectionId] = &[];
    const VERIFY_IGNORE_ALIGNMENT_SECTION_IDS: &'static [OutputSectionId] = &[];

    fn single_part_id(section_id: OutputSectionId) -> Option<PartId> {
        (section_id.as_u32() < crate::output_section_id::regular_section_base::<Self>().as_u32())
            .then(|| PartId::from_usize(section_id.as_usize()))
    }

    fn single_part_output_section_id(part_id: PartId) -> Option<OutputSectionId> {
        (part_id < part_id::regular_part_base::<Self>())
            .then(|| OutputSectionId::from_usize(part_id.as_usize()))
    }

    type File<'data>: ObjectFile<'data, Platform = Self>;
    type FileFlags;
    type SymtabEntry: Symbol;
    type PlatformSpecificSymbol: Copy + PartialEq + Eq + std::fmt::Debug + Send + Sync + 'static;
    type SectionHeader: SectionHeader;
    type SectionFlags: SectionFlags;
    type SectionAttributes: SectionAttributes<Platform = Self>;
    type SectionType: SectionType;
    type SegmentType: SegmentType;
    type ProgramSegmentDef: ProgramSegmentDef;
    type BuiltInSectionDetails: BuiltInSectionDetails;
    type RelocationSections: std::fmt::Debug + Default + Send + Sync + 'static;
    type DynamicEntry: Send + Sync + 'static;
    type DynamicSymbolDefinitionExt: Copy + Send + Sync + std::fmt::Debug + 'static;
    type RelocationInfo: Copy + Send + Sync + 'static;
    type NonAddressableIndexes: NonAddressableIndexes + Send + Sync + 'static;
    type NonAddressableCounts: Default + Send + Sync + 'static;
    type EpilogueLayoutExt: Send + Sync + 'static;
    type GroupLayoutExt: std::fmt::Debug + Send + Sync + 'static;
    type CommonGroupStateExt: Default + std::fmt::Debug + Send + Sync + 'static;
    type StubLibraryLayoutStateExt: std::fmt::Debug + Send + Sync + 'static;
    type StubLibraryLayoutExt: std::fmt::Debug + Send + Sync + 'static;
    type ArchIdentifier: Send + Sync + 'static;
    type Args: Args;
    type ResolutionExt: Default + std::fmt::Debug + Copy + Send + Sync + 'static;
    type SymtabShndxEntry: std::fmt::Debug + Default + Send + Sync + 'static;
    type ResolvedObjectExt<'data>: Default + std::fmt::Debug + Send + Sync;
    type FinaliseSizesExt<'data>: Send + Sync;
    type GcUnit: std::fmt::Debug + Copy + Send + Sync + 'static;

    /// Format-specific fields that form part of a section's identity.
    type SectionIdentityExt: std::fmt::Debug + Copy + Eq + Send + Sync + std::hash::Hash;

    /// An index into the local object's symbol versions.
    type SymbolVersionIndex: Send + Sync + Copy;

    /// Format-specific properties produced by the layout phase.
    type LayoutExt<'data>: Send + Sync;

    type SectionIterator<'a>: Iterator<Item = &'a Self::SectionHeader>
    where
        Self: 'a;
    type DynamicTagValues<'data>: DynamicTagValues<'data>;
    type RelocationList<'data>: RelocationList<'data>;
    type DynamicLayoutStateExt<'data>: Send + Sync + 'data;
    type DynamicLayoutExt<'data>: std::fmt::Debug + Send + Sync + 'data;
    type LayoutResourcesExt<'data>: std::fmt::Debug + Send + Sync + 'data;
    type PreludeLayoutStateExt: std::fmt::Debug + Default + Send + Sync + 'static;
    type PreludeLayoutExt: std::fmt::Debug + Default + Send + Sync + 'static;

    /// Format-specific per-file state used during the layout phase.
    type ObjectLayoutStateExt<'data>: Default + Send + Sync + 'data;

    /// The name of a symbol, possibly with a version.
    type RawSymbolName<'data>: RawSymbolName<'data>;

    /// For platforms that don't support symbol versioning, this can just be the unit type.
    type VersionNames<'data>;

    /// For platforms that don't support symbol versioning, this can just be the unit type.
    type VerneedTable<'data>: VerneedTable<'data>;

    type Layout<'data>;
    type SymbolDb<'data>;
    type Resolver<'data>;
    type ResolutionResources<'data, 'scope>
    where
        'data: 'scope;
    type ObjectLayoutState<'data>;
    type CommonGroupState<'data>;
    type GroupState<'data>;
    type DynamicLayoutState<'data>;
    type PreludeLayoutState<'data>;
    type StubLibraryLayoutState<'data>;
    type GraphResources<'data, 'scope>
    where
        'data: 'scope;
    type LocalWorkQueue;
    type FinaliseLayoutResources<'scope, 'data>
    where
        'data: 'scope;
    type FinaliseSizesResources<'data, 'scope>
    where
        'data: 'scope;
    type ResolutionWriter<'writer, 'out>
    where
        'out: 'writer;
    type DynamicSymbolDefinition<'data>;
    type OutputRecordLayout;
    type SymbolResolutions;
    type LayoutSection;
    type HeaderInfo;
    type Resolution;
    type UnloadedSection;
    type LoadedMetrics;
    type ResolvedObject<'data>;
    type ResolvedDynamic<'data>;
    type ResolvedStubLibrary<'data>;
    type LinkerPlugin<'data>;
    type LoadedPlugin;
    type LtoInput<'data>;
    type Group<'data>;
    type SequencedLinkerScript<'data>;
    type FileLoader<'data, F: crate::fs::FileSystem>;
    type LayoutRulesBuilder<'data>;
    type InternalSymbolsBuilder<'data>;
    type InternalSymDefInfo<'data>;
    type OutputSections<'data>;
    type OutputOrder<'data>;
    type CustomSectionIds;
    type FileWriterOutput<F: crate::fs::FileSystem>;
    type LocationCounter<'data>;
    type SectionOutputInfo<'data>;
    type FileKind;

    fn write_output_file<'data, A: Arch<Platform = Self>, F: FileSystem>(
        output: &Self::FileWriterOutput<F>,
        layout: &Self::Layout<'data>,
    ) -> Result;

    fn maybe_compress_debug_sections<'data, A: Arch<Platform = Self>>(
        _layout: &mut Self::Layout<'data>,
    ) -> Result {
        Ok(())
    }

    /// Possibly initialise a linker plugin if the platform supports it and the arguments specifies
    /// that one should be used.
    fn maybe_init_linker_plugin<'data>(
        _args: &'data Self::Args,
        _linker_plugin_arena: &'data colosseum::sync::Arena<Self::LoadedPlugin>,
        _herd: &'data bumpalo_herd::Herd,
    ) -> Result<Option<Self::LinkerPlugin<'data>>> {
        Ok(None)
    }

    /// Called once all symbols have been read, but only if a linker plugin is active.
    fn plugin_all_symbols_read<'data, F: FileSystem>(
        _plugin: &mut Self::LinkerPlugin<'data>,
        _symbol_db: &mut Self::SymbolDb<'data>,
        _resolver: &mut Self::Resolver<'data>,
        _file_loader: &mut Self::FileLoader<'data, F>,
        _per_symbol_flags: &mut PerSymbolFlags,
        _output_sections: &mut Self::OutputSections<'data>,
        _layout_rules_builder: &mut Self::LayoutRulesBuilder<'data>,
    ) -> Result {
        // Platforms that implement maybe_init_linker_plugin must implement this method too.
        unimplemented!();
    }

    #[allow(dead_code)]
    fn resolve_lto_symbols<'data, 'scope>(
        _obj: &Self::LtoInput<'data>,
        _resources: &'scope Self::ResolutionResources<'data, 'scope>,
        _definitions_out: &mut [SymbolId],
        _scope: &Scope<'scope>,
    ) -> Result {
        Ok(())
    }

    /// Returns whether the supplied file kind is permitted in archives.
    fn is_allowed_in_archive(_kind: Self::FileKind) -> bool {
        false
    }

    /// Number of versions named by a version script, if this platform uses them.
    fn version_script_version_count(_symbol_db: &Self::SymbolDb<'_>) -> u16 {
        0
    }

    /// Returns attributes of the supplied section. This is type+flags and doesn't include other
    /// information like name, size etc.
    fn section_attributes(header: &Self::SectionHeader) -> Self::SectionAttributes;

    /// Validate that the supplied sizes are internally consistent.
    fn validate_sizes(_mem_sizes: &OutputSectionPartMap<u64>) -> Result {
        Ok(())
    }

    /// Implementations can force certain sections to be kept. Only needs to be done for sections
    /// that need to be emitted even if empty.
    fn apply_force_keep_sections(keep_sections: &mut OutputSectionMap<bool>, args: &Self::Args);

    /// Returns whether an input section with zero size destined for the specified output section
    /// should be considered content and thus prevent the output section from being discarded.
    fn is_zero_sized_section_content(section_id: OutputSectionId) -> bool;

    fn built_in_section_details() -> &'static [Self::BuiltInSectionDetails];

    fn finalise_group_layout(memory_offsets: &OutputSectionPartMap<u64>) -> Self::GroupLayoutExt;

    /// Resolves a reference to the frame data section.
    fn frame_data_base_address(memory_offsets: &OutputSectionPartMap<u64>) -> u64;

    /// Aligns the start of a load segment. Platforms may override this to coordinate file and
    /// memory offsets when a segment boundary is introduced.
    fn align_load_segment_start(
        _segment_def: Self::ProgramSegmentDef,
        segment_alignment: Alignment,
        file_offset: &mut usize,
        mem_offset: &mut u64,
    );

    /// Called after GC phase has completed.
    fn post_gc<'data>(
        _groups: &mut [Self::GroupState<'_>],
        _symbol_db: &Self::SymbolDb<'data>,
    ) -> Result {
        Ok(())
    }

    /// The dynamic object will be linked against. This is a chance to perform extra initialisation
    /// of `state`.
    fn activate_dynamic<'data>(
        state: &mut Self::DynamicLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
    );

    fn pre_finalise_sizes_prelude<'scope, 'data>(
        prelude: &mut Self::PreludeLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
        resources: &Self::GraphResources<'data, 'scope>,
    );

    fn finalise_sizes_dynamic<'data>(
        object: &mut Self::DynamicLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
    ) -> Result;

    fn finalise_object_sizes<'data>(
        object: &mut Self::ObjectLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
    );

    fn finalise_object_layout<'data>(
        object: &Self::ObjectLayoutState<'data>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
    );

    /// Return the thunk configuration for the given object file, or `None` if range-extension
    /// thunks are not needed for this file's architecture.
    fn file_thunk_config<'data>(_file: &Self::File<'data>) -> Option<ThunkConfig> {
        None
    }

    fn finalise_layout_dynamic<'data>(
        state: &mut Self::DynamicLayoutState<'data>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resources: &Self::FinaliseLayoutResources<'_, 'data>,
        resolutions_out: &mut Self::ResolutionWriter<'_, '_>,
    ) -> Result<Option<Self::DynamicLayoutExt<'data>>>;

    fn finalise_layout_stub<'data>(
        _state: Self::StubLibraryLayoutState<'data>,
        _memory_offsets: &mut OutputSectionPartMap<u64>,
        _resources: &Self::FinaliseLayoutResources<'_, 'data>,
        _resolutions_out: &mut Self::ResolutionWriter<'_, '_>,
    ) -> Result<Option<Self::StubLibraryLayoutExt>> {
        Ok(None)
    }

    /// Returns the next dynamic symbol index, bumping `memory_offsets` to point to the subsequent
    /// one.
    fn take_dynsym_index(
        memory_offsets: &mut OutputSectionPartMap<u64>,
        section_layouts: &OutputSectionMap<Self::OutputRecordLayout>,
    ) -> Result<u32>;

    fn compute_object_addresses<'data>(
        object: &Self::ObjectLayoutState<'data>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
    );

    fn layout_resources_ext<'data>(
        groups: &[Self::Group<'data>],
    ) -> Self::LayoutResourcesExt<'data>;

    fn gc_unit_for_symbol<'data>(
        object: &Self::File<'data>,
        symbol: &Self::SymtabEntry,
        symbol_index: object::SymbolIndex,
    ) -> Result<Option<Self::GcUnit>>;

    const NEEDS_START_STOP_SECTION_GC: bool = false;

    /// Must be implemented if `NEEDS_START_STOP_SECTION_GC` is true.
    fn gc_unit_for_section(_section_index: object::SectionIndex) -> Self::GcUnit {
        unreachable!("NEEDS_START_STOP_SECTION_GC requires gc_unit_for_section");
    }

    /// Loads GC roots for an object. May also perform platform-specific allocation.
    fn activate_object_gc<'data, 'scope, A: Arch<Platform = Self>>(
        object: &mut Self::ObjectLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
        resources: &'scope Self::GraphResources<'data, 'scope>,
        queue: &mut Self::LocalWorkQueue,
        scope: &Scope<'scope>,
    ) -> Result;

    /// Loads the specified GC unit and queue loading of whatever it references.
    fn load_gc_unit<'data, 'scope, A: Arch<Platform = Self>>(
        object: &mut Self::ObjectLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
        resources: &'scope Self::GraphResources<'data, 'scope>,
        queue: &mut Self::LocalWorkQueue,
        unit: Self::GcUnit,
        scope: &Scope<'scope>,
    ) -> Result;

    /// Calls `load_section_relocations` on `state` for the relocations in `section`.
    fn load_object_section_relocations<'data, 'scope, A: Arch<Platform = Self>>(
        state: &mut Self::ObjectLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
        queue: &mut Self::LocalWorkQueue,
        resources: &'scope Self::GraphResources<'data, '_>,
        section: Self::LayoutSection,
        section_index: object::SectionIndex,
        scope: &Scope<'scope>,
    ) -> Result;

    /// When `--emit-relocs` is set, load the `SHT_REL`/`SHT_RELA` sections that apply to
    /// `section_index` so they survive GC with their target.
    fn load_associated_reloc_sections<'data, 'scope, A: Arch<Platform = Self>>(
        _state: &mut Self::ObjectLayoutState<'data>,
        _common: &mut Self::CommonGroupState<'data>,
        _queue: &mut Self::LocalWorkQueue,
        _resources: &'scope Self::GraphResources<'data, 'scope>,
        _section_index: object::SectionIndex,
        _scope: &Scope<'scope>,
    ) -> Result {
        Ok(())
    }

    fn create_dynamic_symbol_definition<'data>(
        symbol_db: &Self::SymbolDb<'data>,
        symbol_id: SymbolId,
    ) -> Result<Self::DynamicSymbolDefinition<'data>>;

    /// GNU ld emits an empty `STT_OBJECT`/`SHN_ABS` dynamic symbol named after each
    /// named version in `--version-script` (except the BASE/soname version). Glibc
    /// and other consumers look these up in `.dynsym`.
    fn append_version_node_dynamic_symbols<'data>(
        _dynamic_symbol_definitions: &mut Vec<Self::DynamicSymbolDefinition<'data>>,
        _symbol_db: &Self::SymbolDb<'data>,
    ) {
    }

    fn validate_section<'data>(
        _section_info: &Self::SectionOutputInfo<'data>,
        _section_flags: Self::SectionFlags,
        _section_layout: &Self::OutputRecordLayout,
        _merge_target: OutputSectionId,
        _output_sections: &Self::OutputSections<'data>,
        _section_id: OutputSectionId,
    ) -> Result {
        Ok(())
    }

    /// Called when we detect an internal error with allocation in order to try and help determine
    /// what we did wrong. Can optionally return a more helpful error.
    fn verify_resolution_allocation<A: Arch<Platform = Self>>(
        _output_sections: &Self::OutputSections<'_>,
        _output_order: &Self::OutputOrder<'_>,
        _output_kind: OutputKind,
        _mem_sizes: &OutputSectionPartMap<u64>,
        _resolution: &Self::Resolution,
        _args: &Self::Args,
    ) -> Result {
        Ok(())
    }

    /// Updates the list of segments to keep.
    fn update_segment_keep_list(
        program_segments: &ProgramSegments<Self::ProgramSegmentDef>,
        keep_segments: &mut [bool],
        args: &Self::Args,
    );

    fn program_segment_defs() -> &'static [Self::ProgramSegmentDef];

    /// True when program-header `FLAGS()` include write permission (ELF `PF_W`).
    fn phdr_flags_writable(_flags: u64) -> bool {
        false
    }

    /// Returns segment definitions that should be unconditionally emitted without content.
    fn unconditional_segment_defs() -> &'static [Self::ProgramSegmentDef];

    /// Returns whether the specified section should be included in the specified segment.
    fn program_segment_should_include_section(
        segment_def: Self::ProgramSegmentDef,
        section_info: &Self::SectionOutputInfo<'_>,
        section_id: OutputSectionId,
        rosegment: bool,
    ) -> bool;

    fn create_linker_defined_symbols(
        symbols: &mut Self::InternalSymbolsBuilder<'_>,
        output_kind: OutputKind,
        args: &Self::Args,
    );

    fn built_in_section_infos<'data>() -> Vec<Self::SectionOutputInfo<'data>>;

    fn create_finalise_sizes_ext<'data, 'states, 'files, A: Arch<Platform = Self>>(
        args: &Self::Args,
        groups: &'files mut [Self::GroupState<'data>],
        symbol_db: &Self::SymbolDb<'data>,
    ) -> Result<Self::FinaliseSizesExt<'data>>
    where
        'data: 'files,
        'data: 'states;

    fn create_layout_ext<'data>(
        finalise_sizes_ext: Self::FinaliseSizesExt<'data>,
        _resolutions: &Self::SymbolResolutions,
    ) -> Result<Self::LayoutExt<'data>>;

    fn load_exception_frame_data<'data, 'scope, A: Arch<Platform = Self>>(
        object: &mut Self::ObjectLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
        eh_frame_section_index: object::SectionIndex,
        resources: &'scope Self::GraphResources<'data, '_>,
        queue: &mut Self::LocalWorkQueue,
        scope: &Scope<'scope>,
    ) -> Result;

    /// Called when a section is loaded (not GCed). Implementations should process any exception
    /// frame data related to the loaded section.
    fn non_empty_section_loaded<'data, 'scope, A: Arch<Platform = Self>>(
        object: &mut Self::ObjectLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
        queue: &mut Self::LocalWorkQueue,
        unloaded: Self::UnloadedSection,
        resources: &'scope Self::GraphResources<'data, 'scope>,
        scope: &Scope<'scope>,
    ) -> Result;

    fn new_epilogue_layout<'data>(
        args: &Self::Args,
        output_kind: OutputKind,
        dynamic_symbol_definitions: &mut [Self::DynamicSymbolDefinition<'data>],
        group_states: &[Self::GroupState<'data>],
    ) -> Self::EpilogueLayoutExt;

    fn apply_non_addressable_indexes_epilogue(
        counts: &mut Self::NonAddressableCounts,
        state: &mut Self::EpilogueLayoutExt,
    );

    fn apply_non_addressable_indexes<'data, 'groups>(
        symbol_db: &Self::SymbolDb<'data>,
        counts: &Self::NonAddressableCounts,
        mem_sizes_iter: impl Iterator<Item = &'groups mut OutputSectionPartMap<u64>>,
    );

    fn finalise_sizes_epilogue<'data>(
        state: &mut Self::EpilogueLayoutExt,
        mem_sizes: &mut OutputSectionPartMap<u64>,
        dynamic_symbol_definitions: &[Self::DynamicSymbolDefinition<'data>],
        format_specific: &Self::FinaliseSizesExt<'data>,
        symbol_db: &Self::SymbolDb<'data>,
    );

    fn finalise_sizes_all<'data>(
        mem_sizes: &mut OutputSectionPartMap<u64>,
        symbol_db: &Self::SymbolDb<'data>,
    );

    fn apply_late_size_adjustments_epilogue(
        _state: &mut Self::EpilogueLayoutExt,
        _current_sizes: &OutputSectionPartMap<u64>,
        _extra_sizes: &mut OutputSectionPartMap<u64>,
        _dynamic_symbol_defs: &[Self::DynamicSymbolDefinition<'_>],
        _format_specific: &Self::FinaliseSizesExt<'_>,
        _args: &Self::Args,
    ) -> Result {
        Ok(())
    }

    fn apply_late_size_adjustments_prelude(
        _current_sizes: &OutputSectionPartMap<u64>,
        _extra_sizes: &mut OutputSectionPartMap<u64>,
        _format_specific: &Self::FinaliseSizesExt<'_>,
        _args: &Self::Args,
    ) -> Result {
        Ok(())
    }

    /// Returns any extra size needed for the part that currently ends last in
    /// the output file, once its file offset and provisional size are known.
    fn last_part_size_to_extend(
        _record: &Self::OutputRecordLayout,
        _last_part_id: PartId,
    ) -> Result<usize> {
        Ok(0)
    }

    fn finalise_layout_epilogue<'data>(
        epilogue_state: &mut Self::EpilogueLayoutExt,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        symbol_db: &Self::SymbolDb<'data>,
        common_state: &Self::FinaliseSizesExt<'data>,
        dynsym_start_index: u32,
        dynamic_symbol_defs: &[Self::DynamicSymbolDefinition<'_>],
    ) -> Result;

    fn is_symbol_non_interposable<'data>(
        object: &Self::File<'data>,
        args: &Self::Args,
        sym: &Self::SymtabEntry,
        output_kind: OutputKind,
        export_list: Option<&crate::export_list::ExportList>,
        lib_name: &[u8],
        archive_semantics: bool,
        is_undefined: bool,
    ) -> bool;

    /// Given the name of an init/fini section, returns the sort priority, if any.
    fn init_section_priority(_name: &[u8]) -> Option<u16> {
        None
    }

    /// Verifies that it's OK to load a section with the given name. Mostly just used to detect
    /// linker plugin inputs, since we shouldn't be loading those.
    fn verify_allowed_input_section_name(_name: &[u8]) -> Result {
        Ok(())
    }

    /// Allocate space for headers based on segment and section counts.
    fn allocate_header_sizes<'data>(
        prelude: &mut Self::PreludeLayoutState<'data>,
        sizes: &mut OutputSectionPartMap<u64>,
        header_info: &Self::HeaderInfo,
        program_segments: &ProgramSegments<Self::ProgramSegmentDef>,
        output_sections: &Self::OutputSections<'_>,
        resources: &Self::FinaliseSizesResources<'data, '_>,
        args: &Self::Args,
    );

    /// Gives the platform an opportunity to error out if an input stack section is requesting an
    /// executable stack, but that's not permitted due to flags.
    fn validate_stack_section(
        _section: &Self::SectionHeader,
        _object: &impl std::fmt::Display,
        _args: &Self::Args,
    ) -> Result {
        Ok(())
    }

    fn new_stub_library_layout_state_ext<'data>(
        _stub: &Self::ResolvedStubLibrary<'data>,
        _args: &Self::Args,
    ) -> Self::StubLibraryLayoutStateExt {
        unimplemented!()
    }

    fn new_dynamic_layout_state_ext<'data>(
        _file: &Self::ResolvedDynamic<'data>,
        _args: &Self::Args,
    ) -> Self::DynamicLayoutStateExt<'data> {
        unimplemented!()
    }

    fn load_stub_library_symbol(
        _state: &mut Self::StubLibraryLayoutState<'_>,
        _symbol_id: SymbolId,
    ) -> Result {
        Ok(())
    }

    fn finalise_sizes_for_symbol<'data>(
        common: &mut Self::CommonGroupState<'data>,
        symbol_db: &Self::SymbolDb<'data>,
        symbol_id: SymbolId,
        flags: ValueFlags,
    ) -> Result;

    fn allocate_resolution(
        flags: ValueFlags,
        mem_sizes: &mut OutputSectionPartMap<u64>,
        output_kind: OutputKind,
        args: &Self::Args,
    );

    fn allocate_object_symtab_space<'data>(
        state: &Self::ObjectLayoutState<'data>,
        common: &mut Self::CommonGroupState<'data>,
        symbol_db: &Self::SymbolDb<'data>,
        per_symbol_flags: &AtomicPerSymbolFlags,
    ) -> Result;

    fn allocate_thunk_symbol_sizes(
        _sizes: &mut OutputSectionPartMap<u64>,
        _symbols: &[SymbolId],
        _symbol_db: &Self::SymbolDb<'_>,
        _format_specific: &mut Self::CommonGroupStateExt,
    ) {
    }

    fn allocate_internal_symbol(
        symbol_id: SymbolId,
        def_info: &Self::InternalSymDefInfo<'_>,
        sizes: &mut OutputSectionPartMap<u64>,
        symbol_db: &Self::SymbolDb<'_>,
        format_specific: &mut Self::CommonGroupStateExt,
    ) -> Result;

    /// Suffix-merge `.strtab` like GNU ld and move the merged size onto the prelude group.
    fn share_strtab_suffixes<'data>(
        _group_states: &mut [Self::GroupState<'data>],
        _total_sizes: &mut OutputSectionPartMap<u64>,
        _format_specific: &mut Self::FinaliseSizesExt<'data>,
    ) {
    }

    fn allocate_prelude(common: &mut Self::CommonGroupState<'_>, symbol_db: &Self::SymbolDb<'_>);

    fn finalise_prelude_layout<'data>(
        prelude: &Self::PreludeLayoutState<'_>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resources: &Self::FinaliseLayoutResources<'_, 'data>,
    ) -> Result<Self::PreludeLayoutExt>;

    fn create_resolution(
        flags: ValueFlags,
        raw_value: u64,
        dynamic_symbol_index: Option<NonZeroU32>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        args: &Self::Args,
        output_kind: OutputKind,
    ) -> Self::Resolution;

    fn validate_resolution(
        _name: &[u8],
        _resolution: &Self::Resolution,
        _got: &Self::SectionHeader,
        _got_data: &[u8],
    ) -> Result {
        Ok(())
    }

    fn raw_symbol_name<'data>(
        name_bytes: &'data [u8],
        verneed_table: &Self::VerneedTable<'data>,
        symbol_index: object::SymbolIndex,
    ) -> Self::RawSymbolName<'data>;

    fn parse_raw_symbol_name<'data>(name_bytes: &'data [u8]) -> Self::RawSymbolName<'data> {
        <Self::RawSymbolName<'data> as RawSymbolName>::parse(name_bytes)
    }

    fn default_layout_rules(args: &Self::Args) -> Vec<SectionRule<'static>>;

    /// Only called if a linker script that provides custom sections and layout rules is present.
    /// Gives the platform a chance to add extra built-in rules that need to be present even when a
    /// linker script is providing most of the rules.
    fn linker_script_rules_pre_build(_rule_builder: &mut Self::LayoutRulesBuilder<'_>) {}

    fn copy_relocate_symbol<'scope, 'data>(
        _state: &mut Self::DynamicLayoutState<'_>,
        _symbol_id: SymbolId,
        _resources: &Self::GraphResources<'data, 'scope>,
    ) -> Result {
        bail!("Platform does not support copy relocations");
    }

    fn finalise_copy_relocations<'data>(
        _group_states: &mut [Self::GroupState<'data>],
        _symbol_db: &Self::SymbolDb<'data>,
        _symbol_flags: &AtomicPerSymbolFlags,
    ) -> Result {
        Ok(())
    }

    fn build_output_order_and_program_segments<'data>(
        custom: &Self::CustomSectionIds,
        output_kind: OutputKind,
        output_sections: &Self::OutputSections<'data>,
        secondary: &OutputSectionMap<Vec<OutputSectionId>>,
        location_counters: &[Self::LocationCounter<'data>],
    ) -> (
        Self::OutputOrder<'data>,
        ProgramSegments<Self::ProgramSegmentDef>,
    );

    fn build_custom_output_order_and_program_segments<'data>(
        custom: &Self::CustomSectionIds,
        output_kind: OutputKind,
        output_sections: &Self::OutputSections<'data>,
        secondary: &OutputSectionMap<Vec<OutputSectionId>>,
        _linker_scripts: &[&Self::SequencedLinkerScript<'data>],
        location_counters: &[Self::LocationCounter<'data>],
    ) -> Result<(
        Self::OutputOrder<'data>,
        ProgramSegments<Self::ProgramSegmentDef>,
    )> {
        Ok(Self::build_output_order_and_program_segments(
            custom,
            output_kind,
            output_sections,
            secondary,
            location_counters,
        ))
    }

    /// Whether this output section gets a `STT_SECTION` symbol (`-r` and `--emit-relocs`).
    fn will_emit_section_symbol_for_partial_objects(
        _output_sections: &Self::OutputSections<'_>,
        _section_id: OutputSectionId,
    ) -> bool {
        false
    }

    /// Whether the symbol table's first entry (index 0) is a reserved null / sentinel entry that
    /// should be excluded from name resolution. `true` for ELF (`STN_UNDEF`).
    const HAS_NULL_SYMBOL_ENTRY: bool = false;

    /// Used when the linker needs to create a symtab entry from scratch rather than copying one
    /// from an input file.
    fn default_symtab_entry() -> Self::SymtabEntry;

    fn lookup_for_partial_link(
        _section_name: &[u8],
        _section: &Self::SectionHeader,
        _args: &Self::Args,
    ) -> SectionRuleOutcome {
        SectionRuleOutcome::Custom
    }

    fn requires_symtab_shndx(_num_sections: usize) -> bool {
        false
    }

    fn compute_symtab_shndx_section_size(
        _group_sizes: &mut OutputSectionPartMap<u64>,
        _total_sizes: &mut OutputSectionPartMap<u64>,
    ) {
    }

    fn get_sizeof_headers(_header_info: &Self::HeaderInfo) -> u64 {
        0
    }

    /// Scan result for the `.gdb_index` section, if applicable.
    type GdbIndexScanResult<'data>: Send + Sync;

    /// Compute the size of the `.gdb_index` section and return the scan result for the write phase.
    fn compute_gdb_index_size<'data>(
        _groups: &[Self::GroupState<'data>],
    ) -> crate::error::Result<(u64, Option<Self::GdbIndexScanResult<'data>>)> {
        Ok((0, None))
    }

    fn handle_debug_index_section<'data>(
        _obj: &mut Self::ResolvedObject<'data>,
        _section_index: object::SectionIndex,
        _input_section: &'data Self::SectionHeader,
        _member: &bumpalo_herd::Member<'data>,
        _loaded_metrics: &Self::LoadedMetrics,
    ) -> Result {
        Ok(())
    }

    fn new_resolved_object_ext<'data>(
        _symbol_id_range: crate::symbol_db::SymbolIdRange,
        _file_id: crate::input_data::FileId,
    ) -> Self::ResolvedObjectExt<'data> {
        Default::default()
    }

    fn new_object_layout_state_ext<'data>(
        _input: Self::ResolvedObjectExt<'data>,
    ) -> Self::ObjectLayoutStateExt<'data> {
        Default::default()
    }

    fn get_segment_flags_for_section(_section_flags: &Self::SectionFlags) -> u32 {
        0
    }

    /// Constructs the complete identity of an input section, including all format-specific fields
    /// that distinguish sections with the same name.
    fn section_identity<'data>(
        name: SectionName<'data>,
        section: &Self::SectionHeader,
    ) -> SectionIdentity<'data, Self>;

    /// Constructs a section identity with only the section name.
    /// Returns None when the name alone cannot determine the complete section identity.
    fn section_identity_from_name<'data>(
        _name: SectionName<'data>,
    ) -> Option<SectionIdentity<'data, Self>> {
        None
    }

    fn fmt_section_identity(
        name: SectionName<'_>,
        _format_specific: &Self::SectionIdentityExt,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        std::fmt::Display::fmt(&name, f)
    }

    fn apply_linker_script_attributes(
        _linker_script_attributes: &linker_script::SectionAttributes,
        output_attributes: Self::SectionAttributes,
    ) -> Self::SectionAttributes {
        output_attributes
    }

    fn finalise_output_section_alignments(
        _sizes: &OutputSectionPartMap<u64>,
        _output_sections: &mut Self::OutputSections<'_>,
    ) {
    }
}
