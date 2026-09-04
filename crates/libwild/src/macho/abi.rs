use super::MachO;
use super::SinglePartSectionId;
#[allow(unused_imports)]
use super::file::*;
#[allow(unused_imports)]
use super::output::*;
use super::output_section_id;
use super::part_id;
#[allow(unused_imports)]
use super::types::*;
use crate::FileSystem;
use crate::OutputKind;
use crate::alignment;
use crate::alignment::Alignment;
use crate::alignment::MACHO_PAGE_ALIGNMENT;
use crate::args::macho::MachOArgs;
use crate::ensure;
use crate::error;
use crate::error::Result;
use crate::layout;
use crate::layout::HandlerData as _;
use crate::layout::OutputRecordLayout;
use crate::layout::Resolution;
use crate::layout::SectionGcUnit;
use crate::layout::StubLibraryLayoutState;
use crate::layout::SymbolCopyInfo;
use crate::layout::SymbolResolutions;
use crate::layout_rules::SectionKind;
use crate::macho::output_section_id::CHAINED_FIXUP_TABLE;
use crate::macho::output_section_id::CODE_SIGNATURE;
use crate::macho::output_section_id::EXPORTS_TRIE;
use crate::macho::output_section_id::LOAD_COMMANDS;
use crate::macho::output_section_id::STRTAB;
use crate::macho::output_section_id::SYMTAB_GLOBAL;
use crate::macho_writer;
use crate::output_section_id::FILE_HEADER;
use crate::output_section_id::OutputOrderBuilder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionIdentity;
use crate::output_section_id::SectionName;
use crate::output_section_id::SectionOutputInfo;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform;
use crate::platform::ObjectFile;
use crate::platform::SectionAttributes as _;
use crate::program_segments::ProgramSegments;
use crate::resolution;
use crate::symbol_db::SymbolId;
use crate::verbose_timing_phase;
use anyhow::Context;
use itertools::Itertools;
use object::Endianness;
use object::macho;
use object::macho::S_THREAD_LOCAL_VARIABLES;
pub use object::macho::SectionFlags;
use object::read::macho::Section;
use std::slice::Iter;

impl platform::Platform for MachO {
    const NUM_SINGLE_PART_SECTIONS: u32 = SinglePartSectionId::Count as u32;
    const NUM_BUILT_IN_REGULAR_SECTIONS: usize = 0;

    // The macOS kernel caches code signature state by vnode. Reusing a previously executed output's
    // inode after changing its contents can therefore cause the new executable to SIGKILL, even
    // though its new signature verifies successfully.
    const DEFAULT_FILE_REPLACEMENT_MODE: crate::FileReplacementMode = if cfg!(target_os = "macos") {
        crate::FileReplacementMode::UnlinkAndReplace
    } else {
        crate::FileReplacementMode::UpdateInPlaceWithFallback
    };

    const STRTAB_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::STRTAB);
    const SYMTAB_GLOBAL_SECTION_ID: Option<OutputSectionId> =
        Some(output_section_id::SYMTAB_GLOBAL);
    const GOT_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::GOT);
    const PLT_GOT_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::PLT_GOT);

    const VERIFY_IGNORE_ALIGNMENT_SECTION_IDS: &'static [OutputSectionId] =
        &[output_section_id::CODE_SIGNATURE, output_section_id::STRTAB];

    const VERIFY_IGNORE_SECTION_IDS: &'static [OutputSectionId] = &[
        crate::output_section_id::FILE_HEADER,
        output_section_id::LINK_EDIT_SEGMENT,
        output_section_id::LOAD_COMMANDS,
        output_section_id::CHAINED_FIXUP_TABLE,
        output_section_id::EXPORTS_TRIE,
        output_section_id::CODE_SIGNATURE,
    ];

    type File<'data> = File<'data>;
    type FileFlags = u32;
    type SymtabEntry = SymtabEntry;
    type PlatformSpecificSymbol = core::convert::Infallible;
    type SectionHeader = SectionHeader;
    type SectionFlags = SectionFlags;
    type SectionAttributes = SectionAttributes;
    type SectionType = macho::SectionType;
    type SegmentType = ();
    type ProgramSegmentDef = ProgramSegmentDef;
    type BuiltInSectionDetails = BuiltInSectionDetails;
    type RelocationSections = ();
    type DynamicEntry = ();
    type DynamicSymbolDefinitionExt = ();
    type RelocationInfo = object::macho::RelocationInfo;
    type NonAddressableIndexes = NonAddressableIndexes;
    type NonAddressableCounts = ();
    type EpilogueLayoutExt = EpilogueLayoutExt;
    type GroupLayoutExt = ();
    type CommonGroupStateExt = ();
    type StubLibraryLayoutStateExt = DynamicLayoutStateExt;
    type StubLibraryLayoutExt = DynamicLayoutExt;
    type ArchIdentifier = ();
    type Args = MachOArgs;
    type ResolutionExt = ResolutionExt;
    type SymtabShndxEntry = ();
    type SymbolVersionIndex = ();
    type FinaliseSizesExt<'data> = FinaliseSizesExt;
    type LayoutExt<'data> = LayoutExt;
    type GdbIndexScanResult<'data> = ();
    type SectionIterator<'a> = Iter<'a, SectionHeader>;
    type DynamicTagValues<'data> = DynamicTagValues<'data>;
    type RelocationList<'data> = RelocationList<'data>;
    type DynamicLayoutStateExt<'data> = DynamicLayoutStateExt;
    type DynamicLayoutExt<'data> = DynamicLayoutExt;
    type LayoutResourcesExt<'data> = ();
    type PreludeLayoutStateExt = PreludeLayoutExt;
    type PreludeLayoutExt = PreludeLayoutExt;
    type ObjectLayoutStateExt<'data> = ();
    type RawSymbolName<'data> = RawSymbolName<'data>;
    type VersionNames<'data> = ();
    type VerneedTable<'data> = VerneedTable<'data>;
    type ResolvedObjectExt<'data> = ();
    type GcUnit = crate::layout::SectionGcUnit;
    type Layout<'data> = crate::layout::Layout<'data, Self>;
    type SymbolDb<'data> = crate::symbol_db::SymbolDb<'data, Self>;
    type Resolver<'data> = crate::resolution::Resolver<'data, Self>;
    type ResolutionResources<'data, 'scope>
        = crate::resolution::ResolutionResources<'data, 'scope, Self>
    where
        'data: 'scope;
    type ObjectLayoutState<'data> = crate::layout::ObjectLayoutState<'data, Self>;
    type CommonGroupState<'data> = crate::layout::CommonGroupState<'data, Self>;
    type GroupState<'data> = crate::layout::GroupState<'data, Self>;
    type DynamicLayoutState<'data> = crate::layout::DynamicLayoutState<'data, Self>;
    type PreludeLayoutState<'data> = crate::layout::PreludeLayoutState<'data, Self>;
    type StubLibraryLayoutState<'data> = crate::layout::StubLibraryLayoutState<'data, Self>;
    type GraphResources<'data, 'scope>
        = crate::layout::GraphResources<'data, 'scope, Self>
    where
        'data: 'scope;
    type LocalWorkQueue = crate::layout::LocalWorkQueue<Self>;
    type FinaliseLayoutResources<'scope, 'data>
        = crate::layout::FinaliseLayoutResources<'scope, 'data, Self>
    where
        'data: 'scope;
    type FinaliseSizesResources<'data, 'scope>
        = crate::layout::FinaliseSizesResources<'data, 'scope, Self>
    where
        'data: 'scope;
    type ResolutionWriter<'writer, 'out>
        = crate::layout::ResolutionWriter<'writer, 'out, Self>
    where
        'out: 'writer;
    type DynamicSymbolDefinition<'data> = crate::layout::DynamicSymbolDefinition<'data, Self>;
    type OutputRecordLayout = crate::layout::OutputRecordLayout;
    type SymbolResolutions = crate::layout::SymbolResolutions<Self>;
    type LayoutSection = crate::layout::Section;
    type HeaderInfo = crate::layout::HeaderInfo;
    type Resolution = crate::layout::Resolution<Self>;
    type UnloadedSection = crate::resolution::UnloadedSection;
    type LoadedMetrics = crate::resolution::LoadedMetrics;
    type ResolvedObject<'data> = crate::resolution::ResolvedObject<'data, Self>;
    type ResolvedDynamic<'data> = crate::resolution::ResolvedDynamic<'data, Self>;
    type ResolvedStubLibrary<'data> = crate::resolution::ResolvedStubLibrary<'data>;
    type LinkerPlugin<'data> = crate::linker_plugins::LinkerPlugin<'data>;
    type LoadedPlugin = crate::linker_plugins::LoadedPlugin;
    type LtoInput<'data> = crate::linker_plugins::LtoInput<'data>;
    type Group<'data> = crate::grouping::Group<'data, Self>;
    type SequencedLinkerScript<'data> = crate::grouping::SequencedLinkerScript<'data, Self>;
    type FileLoader<'data, F: crate::fs::FileSystem> = crate::input_data::FileLoader<'data, F>;
    type LayoutRulesBuilder<'data> = crate::layout_rules::LayoutRulesBuilder<'data>;
    type InternalSymbolsBuilder<'data> = crate::parsing::InternalSymbolsBuilder<'data, Self>;
    type InternalSymDefInfo<'data> = crate::parsing::InternalSymDefInfo<'data, Self>;

    /// Mach-O sections are associated with a SegmentName, while synthetic regions (FILE_HEADER,
    /// LOAD_COMMANDS, etc.) are not.
    type SectionIdentityExt = Option<SegmentName>;

    const HAS_NULL_SYMBOL_ENTRY: bool = true;

    fn write_output_file<'data, A: platform::Arch<Platform = Self>, F: FileSystem>(
        output: &crate::file_writer::Output<F>,
        layout: &crate::layout::Layout<'data, Self>,
    ) -> Result {
        output.write(layout, macho_writer::write::<A>)
    }

    fn section_attributes(header: &Self::SectionHeader) -> Self::SectionAttributes {
        SectionAttributes::new(
            header.flags.get(LE),
            Some(SegmentName::from_bytes(header.segment_name())),
        )
    }

    fn apply_force_keep_sections(
        _keep_sections: &mut crate::output_section_map::OutputSectionMap<bool>,
        _args: &Self::Args,
    ) {
    }

    fn is_zero_sized_section_content(
        _section_id: crate::output_section_id::OutputSectionId,
    ) -> bool {
        todo!()
    }

    fn built_in_section_details() -> &'static [Self::BuiltInSectionDetails] {
        &SECTION_DEFINITIONS
    }

    fn finalise_group_layout(
        _memory_offsets: &crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) -> Self::GroupLayoutExt {
    }

    fn frame_data_base_address(
        _memory_offsets: &crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) -> u64 {
        todo!()
    }

    fn activate_dynamic<'data>(
        _state: &mut crate::layout::DynamicLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
    ) {
    }

    fn pre_finalise_sizes_prelude<'scope, 'data>(
        _prelude: &mut crate::layout::PreludeLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _resources: &crate::layout::GraphResources<'data, 'scope, Self>,
    ) {
    }

    fn finalise_sizes_dynamic<'data>(
        _object: &mut crate::layout::DynamicLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
    ) -> Result {
        Ok(())
    }

    fn finalise_object_sizes<'data>(
        _object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
    ) {
    }

    fn finalise_object_layout<'data>(
        _object: &crate::layout::ObjectLayoutState<'data, Self>,
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) {
    }

    fn finalise_layout_dynamic<'data>(
        state: &mut Self::DynamicLayoutState<'data>,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        resources: &Self::FinaliseLayoutResources<'_, 'data>,
        resolutions_out: &mut Self::ResolutionWriter<'_, '_>,
    ) -> Result<Option<Self::DynamicLayoutExt<'data>>> {
        layout::default_create_resolutions(
            memory_offsets,
            resolutions_out,
            resources,
            state.symbol_id_range,
        )?;

        create_dynamic_layout_ext(state.file_id(), resources)
    }

    fn finalise_layout_stub<'data>(
        state: layout::StubLibraryLayoutState<'data, Self>,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        resources: &crate::layout::FinaliseLayoutResources<'_, 'data, Self>,
        resolutions_out: &mut crate::layout::ResolutionWriter<Self>,
    ) -> Result<Option<Self::StubLibraryLayoutExt>> {
        layout::default_create_resolutions(
            memory_offsets,
            resolutions_out,
            resources,
            state.symbol_id_range,
        )?;

        create_dynamic_layout_ext(state.file_id(), resources)
    }

    fn take_dynsym_index(
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _section_layouts: &crate::output_section_map::OutputSectionMap<
            crate::layout::OutputRecordLayout,
        >,
    ) -> Result<u32> {
        todo!()
    }

    fn compute_object_addresses<'data>(
        _object: &crate::layout::ObjectLayoutState<'data, Self>,
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) {
        todo!()
    }

    fn layout_resources_ext<'data>(
        _groups: &[Self::Group<'data>],
    ) -> Self::LayoutResourcesExt<'data> {
    }

    fn gc_unit_for_symbol<'data>(
        object: &Self::File<'data>,
        symbol: &Self::SymtabEntry,
        symbol_index: object::SymbolIndex,
    ) -> Result<Option<Self::GcUnit>> {
        Ok(object
            .symbol_section(symbol, symbol_index)?
            .map(SectionGcUnit::new))
    }

    fn activate_object_gc<'data, 'scope, A: platform::Arch<Platform = Self>>(
        object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        resources: &'scope crate::layout::GraphResources<'data, 'scope, Self>,
        queue: &mut crate::layout::LocalWorkQueue<Self>,
        scope: &rayon::Scope<'scope>,
    ) -> Result {
        object.activate_section_gc::<A>(common, resources, queue, scope)
    }

    fn load_gc_unit<'data, 'scope, A: platform::Arch<Platform = Self>>(
        object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        resources: &'scope crate::layout::GraphResources<'data, 'scope, Self>,
        queue: &mut crate::layout::LocalWorkQueue<Self>,
        unit: Self::GcUnit,
        scope: &rayon::Scope<'scope>,
    ) -> Result {
        object.handle_section_load_request::<A>(
            common,
            resources,
            queue,
            unit.section_index(),
            scope,
        )
    }

    fn load_object_section_relocations<'data, 'scope, A: platform::Arch<Platform = Self>>(
        state: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        queue: &mut crate::layout::LocalWorkQueue<Self>,
        resources: &'scope crate::layout::GraphResources<'data, '_, Self>,
        _section: crate::layout::Section,
        section_index: object::SectionIndex,
        scope: &rayon::Scope<'scope>,
    ) -> Result {
        // TODO
        for rel in state.relocations(section_index)?.relocations {
            process_relocation::<A>(state, rel, section_index, resources, queue, scope)?;
        }
        Ok(())
    }

    fn create_dynamic_symbol_definition<'data>(
        symbol_db: &Self::SymbolDb<'data>,
        symbol_id: crate::symbol_db::SymbolId,
    ) -> Result<Self::DynamicSymbolDefinition<'data>> {
        Ok(crate::layout::DynamicSymbolDefinition {
            symbol_id,
            name: symbol_db.symbol_name(symbol_id)?.bytes(),
            format_specific: (),
        })
    }

    fn update_segment_keep_list(
        _program_segments: &crate::program_segments::ProgramSegments<Self::ProgramSegmentDef>,
        _keep_segments: &mut [bool],
        _args: &Self::Args,
    ) {
    }

    fn program_segment_defs() -> &'static [Self::ProgramSegmentDef] {
        &[]
    }

    fn unconditional_segment_defs() -> &'static [Self::ProgramSegmentDef] {
        &[]
    }

    fn program_segment_should_include_section(
        segment_def: Self::ProgramSegmentDef,
        section_info: &crate::output_section_id::SectionOutputInfo<Self>,
        section_id: crate::output_section_id::OutputSectionId,
        _rosegment: bool,
    ) -> bool {
        match (section_id, section_info.kind) {
            (FILE_HEADER | LOAD_COMMANDS, _) => segment_def.name == SegmentName::TEXT,
            (STRTAB | CHAINED_FIXUP_TABLE | SYMTAB_GLOBAL | EXPORTS_TRIE | CODE_SIGNATURE, _) => {
                segment_def.name == SegmentName::LINKEDIT
            }
            (_, SectionKind::Primary(identity)) => {
                identity.format_specific() == Some(segment_def.name)
            }
            (_, SectionKind::Secondary(_)) => false,
        }
    }

    fn create_linker_defined_symbols(
        _symbols: &mut crate::parsing::InternalSymbolsBuilder<Self>,
        _output_kind: crate::output_kind::OutputKind,
        _args: &Self::Args,
    ) {
    }

    fn built_in_section_infos<'data>()
    -> Vec<crate::output_section_id::SectionOutputInfo<'data, Self>> {
        SECTION_DEFINITIONS
            .iter()
            .map(|d| {
                let segment = match d.kind {
                    SectionKind::Primary(identity) => identity.format_specific(),
                    SectionKind::Secondary(_) => None,
                };
                SectionOutputInfo {
                    section_attributes: SectionAttributes::new(d.section_flags, segment),
                    kind: d.kind,
                    min_alignment: d.min_alignment,
                    location_info: None,
                    secondary_order: None,
                    region_name: None,
                    fill: None,
                    phdrs: Vec::new(),
                    input_order: false,
                }
            })
            .collect()
    }

    fn create_finalise_sizes_ext<'data, 'states, 'files, A: platform::Arch<Platform = Self>>(
        _args: &Self::Args,
        groups: &'files mut [layout::GroupState<'data, Self>],
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) -> Result<Self::FinaliseSizesExt<'data>>
    where
        'data: 'files,
        'data: 'states,
    {
        let mut imported_libraries = Vec::new();
        let mut imported_symbols = Vec::new();

        for group in groups {
            for file in &group.files {
                match file {
                    layout::FileLayoutState::StubLibrary(state) => {
                        if state.format_specific.loaded {
                            imported_libraries.push(state.file_id());
                        }
                        imported_symbols
                            .extend_from_slice(state.format_specific.imported_symbols.as_slice());
                    }
                    layout::FileLayoutState::Dynamic(state) => {
                        if state.format_specific.loaded {
                            imported_libraries.push(state.file_id());
                        }
                        imported_symbols
                            .extend_from_slice(state.format_specific.imported_symbols.as_slice());
                    }
                    _ => {}
                }
            }
        }

        Ok(FinaliseSizesExt {
            imported_libraries,
            imported_symbols,
        })
    }

    fn create_layout_ext<'data>(
        finalise_sizes_ext: Self::FinaliseSizesExt<'data>,
        resolutions: &SymbolResolutions<Self>,
    ) -> Result<Self::LayoutExt<'data>> {
        let mut layout_ext = LayoutExt::default();

        let imported_symbols = finalise_sizes_ext
            .imported_symbols
            .iter()
            .map(|&symbol_id| {
                let resolution = resolutions
                    .get(symbol_id)
                    .with_context(|| "missing resolution for a stub library symbol".to_string())?;

                let got_address = resolution
                    .format_specific
                    .got_address
                    .ok_or_else(|| error!("missing GOT entry for a stub library symbol"))?;

                Ok(ImportedSymbolWithResolution {
                    symbol_id,
                    got_address,
                    plt_address: resolution.format_specific.plt_address,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        layout_ext.imported_symbols = imported_symbols
            .into_iter()
            .sorted_by_key(|symbol| symbol.got_address)
            .collect();

        Ok(layout_ext)
    }

    fn load_exception_frame_data<'data, 'scope, A: platform::Arch<Platform = Self>>(
        _object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _eh_frame_section_index: object::SectionIndex,
        _resources: &'scope crate::layout::GraphResources<'data, '_, Self>,
        _queue: &mut crate::layout::LocalWorkQueue<Self>,
        _scope: &rayon::Scope<'scope>,
    ) -> Result {
        todo!()
    }

    fn non_empty_section_loaded<'data, 'scope, A: platform::Arch<Platform = Self>>(
        _object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _queue: &mut crate::layout::LocalWorkQueue<Self>,
        _unloaded: crate::resolution::UnloadedSection,
        _resources: &'scope crate::layout::GraphResources<'data, 'scope, Self>,
        _scope: &rayon::Scope<'scope>,
    ) -> Result {
        Ok(())
    }

    fn new_epilogue_layout<'data>(
        _args: &Self::Args,
        _output_kind: crate::output_kind::OutputKind,
        _dynamic_symbol_definitions: &mut [crate::layout::DynamicSymbolDefinition<'data, Self>],
        group_states: &[layout::GroupState<'data, Self>],
    ) -> Self::EpilogueLayoutExt {
        verbose_timing_phase!("Gather imported symbol IDs");

        let imported_symbols = group_states
            .iter()
            .flat_map(|group| {
                group.files.iter().flat_map(|file| match file {
                    layout::FileLayoutState::StubLibrary(file) => {
                        file.format_specific.imported_symbols.as_slice()
                    }
                    layout::FileLayoutState::Dynamic(file) => {
                        file.format_specific.imported_symbols.as_slice()
                    }
                    _ => &[],
                })
            })
            .copied()
            .collect();

        EpilogueLayoutExt { imported_symbols }
    }

    fn apply_non_addressable_indexes_epilogue(
        _counts: &mut Self::NonAddressableCounts,
        _state: &mut Self::EpilogueLayoutExt,
    ) {
    }

    fn apply_non_addressable_indexes<'data, 'groups>(
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        _counts: &Self::NonAddressableCounts,
        _mem_sizes_iter: impl Iterator<
            Item = &'groups mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        >,
    ) {
    }

    fn finalise_sizes_epilogue<'data>(
        state: &mut Self::EpilogueLayoutExt,
        mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        dynamic_symbol_definitions: &[crate::layout::DynamicSymbolDefinition<'data, Self>],
        _format_specific: &Self::FinaliseSizesExt<'data>,
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) {
        let mut fixup_table_size = CHAINED_FIXUP_TABLE_BASE_SIZE;

        fixup_table_size += state
            .imported_symbols
            .iter()
            .map(|&s| {
                CHAINED_FIXUP_IMPORT_SIZE
                    + symbol_db.symbol_name(s).unwrap().bytes().len() as u64
                    + 1
            })
            .sum::<u64>();

        // Chained fixups record start information per page. At this point the final GOT size is
        // known, so reserve the fixup table entries needed to describe the GOT pages.
        fixup_table_size += CHAINED_FIXUP_PAGE_START_SIZE
            * (state.imported_symbols.len() as u64).div_ceil(MACHO_PAGE_ALIGNMENT.value());

        mem_sizes.increment(
            part_id::CHAINED_FIXUP_TABLE,
            alignment::USIZE.align_up(fixup_table_size),
        );

        // Currently we determine the output file size before we assign symbol addresses. This lets
        // us do file creation in parallel with address assignment, however it means that we can't
        // take addresses into account when determining section sizes. The export trie, due to using
        // uleb128 encoding for addresses, needs addresses in order to determine an exact size. We
        // work around this for now by assuming all addresses will be u64::MAX. This gives us an
        // upper bound on how large the trie will be, but wastes some space in the file. TODO:
        // Figure out a good way to fix this.
        let mut exports = dynamic_symbol_definitions
            .iter()
            .map(|symbol| crate::trie::Symbol {
                name: symbol.name,
                address: u64::MAX,
                flags: object::macho::ExportSymbolFlags(0),
            })
            .collect_vec();

        mem_sizes.increment(
            part_id::EXPORTS_TRIE,
            crate::trie::build(&mut exports).len() as u64,
        );
    }

    fn finalise_sizes_all<'data>(
        _mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) {
    }

    fn finalise_layout_epilogue<'data>(
        _epilogue_state: &mut Self::EpilogueLayoutExt,
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        _format_specific: &Self::FinaliseSizesExt<'data>,
        _dynsym_start_index: u32,
        _dynamic_symbol_defs: &[crate::layout::DynamicSymbolDefinition<Self>],
    ) -> Result {
        Ok(())
    }

    fn is_symbol_non_interposable<'data>(
        _object: &Self::File<'data>,
        _args: &Self::Args,
        _sym: &Self::SymtabEntry,
        _output_kind: crate::output_kind::OutputKind,
        _export_list: Option<&crate::export_list::ExportList>,
        _lib_name: &[u8],
        _archive_semantics: bool,
        _is_undefined: bool,
    ) -> bool {
        // TODO
        true
    }

    fn allocate_header_sizes<'data>(
        prelude: &mut crate::layout::PreludeLayoutState<'data, Self>,
        sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        header_info: &crate::layout::HeaderInfo,
        program_segments: &ProgramSegments<Self::ProgramSegmentDef>,
        output_sections: &crate::output_section_id::OutputSections<Self>,
        resources: &layout::FinaliseSizesResources<'data, '_, Self>,
        args: &Self::Args,
    ) {
        sizes.increment(crate::part_id::FILE_HEADER, size_of::<FileHeader>() as u64);

        let mut allocate_load_cmd = |command_size| {
            sizes.increment(part_id::LOAD_COMMANDS, command_size as u64);
            prelude.format_specific.load_command_count += 1;
        };

        // Separately emitted __PAGEZERO.
        allocate_load_cmd(size_of::<SegmentCommand>());

        for &segment_id in &header_info.active_segment_ids {
            let segment = program_segments.segment_def(segment_id);
            allocate_load_cmd(
                size_of::<SegmentCommand>()
                    + size_of::<SectionEntry>()
                        * count_sections_for_segment(output_sections, *segment),
            );
        }

        if resources.symbol_db.output_kind.is_executable() {
            allocate_load_cmd(size_of::<EntryPointCommand>());
        }
        allocate_load_cmd(
            (size_of::<DylinkerCommand>() + DYLINKER_PATH.len())
                .next_multiple_of(MACHO_COMMAND_ALIGNMENT),
        );

        prelude.format_specific.imported_library_file_ids =
            resources.format_specific.imported_libraries.clone();

        prelude.format_specific.load_dylib_command_sizes = prelude
            .format_specific
            .imported_library_file_ids
            .iter()
            .map(|&file_id| load_dylib_command_size(install_name(file_id, resources.symbol_db)))
            .collect();
        let load_dylib_command_sizes = prelude.format_specific.load_dylib_command_sizes.clone();
        for command_size in load_dylib_command_sizes {
            allocate_load_cmd(command_size);
        }

        allocate_load_cmd(size_of::<DyldChainedFixupsCommand>());
        if resources.symbol_db.output_kind.needs_dynsym() {
            allocate_load_cmd(size_of::<object::macho::LinkeditDataCommand<Endianness>>());
        }
        allocate_load_cmd(size_of::<SymtabCommand>());
        allocate_load_cmd(size_of::<CodeSignatureCommand>());
        allocate_load_cmd(size_of::<UuidCommand>());
        if args.platform_version.is_some() {
            allocate_load_cmd(size_of::<BuildVersionCommand>());
        }
    }

    fn new_stub_library_layout_state_ext<'data>(
        _stub: &resolution::ResolvedStubLibrary<'data>,
        args: &Self::Args,
    ) -> Self::StubLibraryLayoutStateExt {
        DynamicLayoutStateExt::new(args)
    }

    fn new_dynamic_layout_state_ext<'data>(
        _file: &Self::ResolvedDynamic<'data>,
        args: &Self::Args,
    ) -> Self::DynamicLayoutStateExt<'data> {
        DynamicLayoutStateExt::new(args)
    }

    fn load_stub_library_symbol<'data>(
        state: &mut StubLibraryLayoutState<Self>,
        symbol_id: SymbolId,
    ) -> Result {
        state.format_specific.loaded = true;
        state.format_specific.imported_symbols.push(symbol_id);

        Ok(())
    }

    fn finalise_sizes_for_symbol<'data>(
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        _symbol_id: crate::symbol_db::SymbolId,
        _flags: crate::value_flags::ValueFlags,
    ) -> Result {
        Ok(())
    }

    fn allocate_resolution(
        flags: crate::value_flags::ValueFlags,
        mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _output_kind: crate::output_kind::OutputKind,
        _args: &Self::Args,
    ) {
        if flags.is_dynamic() && flags.needs_plt() {
            mem_sizes.increment(part_id::PLT_GOT, PLT_ENTRY_SIZE);
        }
        if flags.is_dynamic() && flags.needs_got() {
            mem_sizes.increment(part_id::GOT, GOT_ENTRY_SIZE);
        }
    }

    fn allocate_object_symtab_space<'data>(
        state: &crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        per_symbol_flags: &crate::value_flags::AtomicPerSymbolFlags,
    ) -> Result {
        let mut num_globals = 0;
        let mut strings_size = 0;
        for ((sym_index, sym), flags) in state
            .object
            .enumerate_symbols()
            .zip(per_symbol_flags.range(state.symbol_id_range))
        {
            let symbol_id = state.symbol_id_range.input_to_id(sym_index);
            if let Some(info) = SymbolCopyInfo::new(
                state.object,
                sym_index,
                sym,
                symbol_id,
                symbol_db,
                flags.get(),
                &state.sections,
            ) {
                num_globals += 1;
                strings_size += info.name.len() + 1;
            }
        }
        let entry_size = size_of::<SymtabEntry>() as u64;
        common.allocate(part_id::SYMTAB_GLOBAL, num_globals * entry_size);
        common.allocate(part_id::STRTAB, strings_size as u64);

        Ok(())
    }

    fn allocate_internal_symbol(
        _symbol_id: crate::symbol_db::SymbolId,
        _def_info: &crate::parsing::InternalSymDefInfo<Self>,
        _sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _symbol_db: &crate::symbol_db::SymbolDb<Self>,
        _format_specific: &mut Self::CommonGroupStateExt,
    ) -> Result {
        todo!()
    }

    fn allocate_prelude(
        common: &mut crate::layout::CommonGroupState<Self>,
        symbol_db: &crate::symbol_db::SymbolDb<Self>,
    ) {
        // Allocate one extra character as n_strx == 0 is treated as unnamed.
        common.allocate(part_id::STRTAB, 1);
        common.allocate(
            part_id::CODE_SIGNATURE,
            CS_HEADERS_SIZE + code_signature_padded_identifier_size(symbol_db.args),
        );
    }

    fn finalise_prelude_layout<'data>(
        prelude: &crate::layout::PreludeLayoutState<Self>,
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _resources: &crate::layout::FinaliseLayoutResources<'_, 'data, Self>,
    ) -> Result<Self::PreludeLayoutExt> {
        Ok(prelude.format_specific.clone())
    }

    fn create_resolution(
        flags: crate::value_flags::ValueFlags,
        raw_value: u64,
        dynamic_symbol_index: Option<std::num::NonZeroU32>,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _args: &<Self as crate::platform::Platform>::Args,
        _output_kind: crate::OutputKind,
    ) -> crate::layout::Resolution<Self> {
        let mut resolution: Resolution<MachO> = Resolution {
            raw_value,
            dynamic_symbol_index,
            format_specific: ResolutionExt {
                got_address: None,
                plt_address: None,
            },
            flags,
        };

        if flags.needs_plt() {
            let plt_address = allocate_plt(memory_offsets);
            resolution.raw_value = plt_address.get();
            resolution.format_specific.plt_address = Some(plt_address);
            resolution.format_specific.got_address = Some(allocate_got(memory_offsets));
        } else if flags.needs_got() {
            let got_address = allocate_got(memory_offsets);
            resolution.raw_value = got_address.get();
            resolution.format_specific.got_address = Some(got_address);
        }

        resolution
    }

    fn raw_symbol_name<'data>(
        name_bytes: &'data [u8],
        _verneed_table: &Self::VerneedTable<'data>,
        _symbol_index: object::SymbolIndex,
    ) -> Self::RawSymbolName<'data> {
        RawSymbolName { name: name_bytes }
    }

    fn default_layout_rules(_args: &Self::Args) -> Vec<crate::layout_rules::SectionRule<'static>> {
        DEFAULT_SECTION_RULES.to_vec()
    }

    fn build_output_order_and_program_segments<'data>(
        custom: &crate::output_section_id::CustomSectionIds,
        output_kind: OutputKind,
        output_sections: &crate::output_section_id::OutputSections<'data, Self>,
        secondary: &crate::output_section_map::OutputSectionMap<
            Vec<crate::output_section_id::OutputSectionId>,
        >,
        _location_counters: &[crate::layout_rules::LocationCounter<'data>],
    ) -> (
        crate::output_section_id::OutputOrder<'data>,
        crate::program_segments::ProgramSegments<Self::ProgramSegmentDef>,
    ) {
        // TODO: Order sections within each segment according to Mach-O conventions.
        let arbitrary_segments: Vec<SegmentName> = output_sections
            .ids_with_info()
            .filter_map(|(_, info)| match info.kind {
                SectionKind::Primary(identity) => identity.format_specific(),
                SectionKind::Secondary(_) => None,
            })
            .filter(|name| {
                !matches!(
                    *name,
                    SegmentName::PAGEZERO
                        | SegmentName::TEXT
                        | SegmentName::DATA_CONST
                        | SegmentName::DATA
                        | SegmentName::LINKEDIT
                )
            })
            .unique()
            .collect();

        let segment_defs = [
            SegmentName::TEXT,
            SegmentName::DATA_CONST,
            SegmentName::DATA,
        ]
        .into_iter()
        .chain(arbitrary_segments.iter().copied())
        .chain([SegmentName::LINKEDIT])
        .map(ProgramSegmentDef::new)
        .collect();

        let mut builder = OutputOrderBuilder::<Self>::new(
            segment_defs,
            output_kind,
            output_sections,
            secondary,
            false,
            &[],
        );

        // File header and all load commands.
        builder.add_section(crate::output_section_id::FILE_HEADER);
        builder.add_section(output_section_id::LOAD_COMMANDS);

        // Content of the sections (e.g. __text, __data).
        add_sections_in_segment(
            &mut builder,
            output_sections,
            &custom.exec,
            SegmentName::TEXT,
        );

        builder.add_section(output_section_id::PLT_GOT);
        add_sections_in_segment(&mut builder, output_sections, &custom.ro, SegmentName::TEXT);
        builder.add_section(output_section_id::GOT);

        for segment in [SegmentName::DATA_CONST, SegmentName::DATA] {
            add_sections_in_segment(&mut builder, output_sections, &custom.exec, segment);
            add_sections_in_segment(&mut builder, output_sections, &custom.ro, segment);
            add_sections_in_segment(&mut builder, output_sections, &custom.data, segment);
            if segment == SegmentName::DATA {
                add_sections_in_segment(&mut builder, output_sections, &custom.tdata, segment);
                add_sections_in_segment(&mut builder, output_sections, &custom.tbss, segment);
            }
            add_sections_in_segment(&mut builder, output_sections, &custom.bss, segment);
        }

        // Arbitrary segment sections are added in first-seen order.
        for segment in arbitrary_segments {
            for (section_id, info) in output_sections.ids_with_info() {
                if matches!(info.kind, SectionKind::Primary(identity) if identity.format_specific() == Some(segment))
                {
                    builder.add_section(section_id);
                }
            }
        }

        // The rest (e.g. symbol table, string table).
        builder.add_section(output_section_id::STRTAB);
        builder.add_section(output_section_id::CHAINED_FIXUP_TABLE);
        builder.add_section(output_section_id::EXPORTS_TRIE);
        builder.add_section(output_section_id::SYMTAB_GLOBAL);
        builder.add_section(output_section_id::CODE_SIGNATURE);

        builder.build()
    }

    fn align_load_segment_start(
        _segment_def: ProgramSegmentDef,
        segment_alignment: Alignment,
        file_offset: &mut usize,
        mem_offset: &mut u64,
    ) {
        *file_offset = segment_alignment.align_up(*file_offset as u64) as usize;
        *mem_offset = segment_alignment.align_up(*mem_offset);
    }

    fn default_symtab_entry() -> Self::SymtabEntry {
        Self::SymtabEntry {
            n_strx: Default::default(),
            n_type: Default::default(),
            n_sect: Default::default(),
            n_desc: Default::default(),
            n_value: Default::default(),
        }
    }

    fn last_part_size_to_extend(
        record: &OutputRecordLayout,
        last_part_id: PartId,
    ) -> Result<usize> {
        ensure!(
            last_part_id == part_id::CODE_SIGNATURE,
            "code signature must be last part_id"
        );
        // The CODE_SIGNATURE size depends on the final file size, excluding the
        // signature itself. Compute it after layout because there is one SHA hash
        // per file block (4 KiB) covered by the signature.
        Ok(record.file_offset.div_ceil(CS_BLOCK_SIZE) * CS_HASH_SIZE as usize)
    }

    fn is_allowed_in_archive(kind: crate::file_kind::FileKind) -> bool {
        kind == crate::file_kind::FileKind::MachOObject
    }

    fn section_identity<'data>(
        name: SectionName<'data>,
        section: &Self::SectionHeader,
    ) -> SectionIdentity<'data, Self> {
        SectionIdentity::new(name, Some(SegmentName::from_bytes(section.segment_name())))
    }

    fn fmt_section_identity(
        section_name: SectionName<'_>,
        segment_name: &Self::SectionIdentityExt,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match segment_name {
            Some(segment_name) => write!(f, "{segment_name},{section_name}"),
            None => write!(f, "{section_name}"),
        }
    }

    fn finalise_output_section_alignments(
        sizes: &OutputSectionPartMap<u64>,
        output_sections: &mut crate::output_section_id::OutputSections<'_, Self>,
    ) {
        let tlv_sections = output_sections
            .ids_with_info()
            .filter_map(|(section_id, info)| info.section_attributes.is_tls().then_some(section_id))
            .collect_vec();

        let tlv_descriptors = output_sections
            .ids_with_info()
            .filter_map(|(section_id, info)| {
                (info.section_attributes.ty() == S_THREAD_LOCAL_VARIABLES).then_some(section_id)
            })
            .collect_vec();

        let max_align = tlv_sections
            .iter()
            .map(|&section_id| {
                sizes.max_alignment(section_id.part_id_range::<MachO>(), output_sections)
            })
            .max();

        if let Some(max_align) = max_align {
            for section_id in tlv_sections {
                output_sections.bump_min_alignment(section_id, max_align);
            }
        }

        for section_id in tlv_descriptors {
            output_sections.bump_min_alignment(section_id, alignment::USIZE);
        }
    }
}
