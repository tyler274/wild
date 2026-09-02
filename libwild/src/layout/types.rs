use super::graph::*;
use super::script::*;
use super::section_debug;
use super::sections::*;
use super::sizes::*;
use crate::OutputKind;
use crate::alignment;
use crate::alignment::Alignment;
use crate::bail;
use crate::compression::CompressedSection;
use crate::debug_assert_bail;
use crate::error::Context;
use crate::error::Error;
use crate::error::Result;
use crate::expression_eval::ResolvedLocationCounter;
use crate::grouping::Group;
use crate::grouping::SequencedInputObject;
use crate::input_data::FileId;
use crate::input_data::InputRef;
use crate::input_data::PRELUDE_FILE_ID;
use crate::input_section_id::SectionIdRange;
use crate::linker_script::Expression;
use crate::output_section_id;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::SymbolPlacement;
use crate::part_id::PartId;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::ProgramSegmentDef as _;
use crate::platform::RelaxSymbolInfo;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionFlags as _;
use crate::platform::SectionHeader as _;
use crate::platform::Symbol as _;
use crate::program_segments::ProgramSegmentId;
use crate::program_segments::ProgramSegments;
use crate::resolution;
use crate::resolution::NotLoaded;
use crate::resolution::ResolvedGroup;
use crate::resolution::ScriptSortedSectionDetail;
use crate::resolution::SectionSlot;
use crate::resolution::UnloadedSection;
use crate::sharding::ShardKey;
use crate::string_merging::MergedStringStartAddresses;
use crate::string_merging::MergedStringsSection;
use crate::string_merging::get_merged_string_output_address;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolDebug;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolIdRange;
use crate::thunks::ThunkBlockId;
use crate::thunks::ThunkLayoutBuilder;
use crate::timing_phase;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::FlagsForSymbol as _;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use crossbeam_queue::ArrayQueue;
use crossbeam_queue::SegQueue;
use hashbrown::HashMap;
use hashbrown::HashSet;
use itertools::Itertools;
use linker_utils::relaxation::RelaxDeltaMap;
use linker_utils::relaxation::opt_input_to_output;
use object::SectionIndex;
use rayon::Scope;
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt::Display;
use std::mem::replace;
use std::mem::size_of;
use std::mem::swap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;

pub(crate) struct FinaliseSizesResources<'data, 'scope, P: Platform> {
    pub(crate) dynamic_symbol_definitions: &'scope [DynamicSymbolDefinition<'data, P>],
    pub(crate) symbol_db: &'scope SymbolDb<'data, P>,
    pub(crate) merged_strings: &'scope OutputSectionMap<MergedStringsSection<'data>>,
    pub(crate) format_specific: &'scope P::FinaliseSizesExt<'data>,
    pub(crate) script_sorted_sections: &'scope [InputSortedSection],
}

/// Information about what goes where. Also includes relocation data, since that's computed at the
/// same time.
#[derive(Debug)]
pub struct Layout<'data, P: Platform> {
    pub(crate) symbol_db: SymbolDb<'data, P>,
    pub(crate) symbol_resolutions: SymbolResolutions<P>,
    pub(crate) got_relr_n: u64,
    pub(crate) section_part_layouts: OutputSectionPartMap<OutputRecordLayout>,

    pub(crate) section_layouts: OutputSectionMap<OutputRecordLayout>,

    /// This is like `section_layouts`, but where secondary sections are merged into their primary
    /// section. Values for secondary sections are reset to 0 and should not be used.
    pub(crate) merged_section_layouts: OutputSectionMap<OutputRecordLayout>,

    pub(crate) group_layouts: Vec<GroupLayout<'data, P>>,
    pub(crate) segment_layouts: SegmentLayouts,
    pub(crate) output_sections: OutputSections<'data, P>,
    pub(crate) program_segments: ProgramSegments<P::ProgramSegmentDef>,
    pub(crate) output_order: OutputOrder<'data>,
    pub(crate) non_addressable_counts: P::NonAddressableCounts,
    pub(crate) merged_strings: OutputSectionMap<MergedStringsSection<'data>>,
    pub(crate) merged_string_start_addresses: MergedStringStartAddresses,
    pub(crate) relocation_statistics: OutputSectionMap<AtomicU64>,
    pub(crate) has_static_tls: bool,
    pub(crate) has_variant_pcs: bool,
    pub(crate) per_symbol_flags: PerSymbolFlags,
    pub(crate) dynamic_symbol_definitions: Vec<DynamicSymbolDefinition<'data, P>>,
    pub(crate) format_specific: P::LayoutExt<'data>,
    /// Thunk address maps indexed by ThunkBlockId. Each entry maps SymbolId to the memory address
    /// of the thunk for that symbol within the block.
    pub(crate) thunk_block_addresses: Vec<BTreeMap<SymbolId, u64>>,

    pub(crate) compressed_debug_sections: OutputSectionMap<Option<CompressedSection>>,
    pub(crate) gdb_index_data: Option<P::GdbIndexScanResult<'data>>,
    pub(crate) script_sorted_sections: Vec<InputSortedSection>,
    pub(crate) resolved_location_counters: Vec<ResolvedLocationCounter>,
    /// Object FileIds whose allocatable section payloads can be left in the existing output during
    /// an incremental update. Empty unless `--incremental` is doing an in-place rewrite.
    pub(crate) incremental_skip_payloads: HashSet<FileId>,
    /// Sites that applied a relocation, keyed by defined symbol. Empty when not incremental.
    pub(crate) incremental_reverse_relocs: Mutex<crate::incremental::ReverseRelocIndex>,
    /// Loaded previous reverse-reloc index + resolutions for patching skipped objects.
    pub(crate) incremental_patch: Option<crate::incremental::IncrementalPatchJob>,
}

#[derive(Debug, Default)]
pub(crate) struct SegmentLayouts {
    /// The layout of each of our segments. Segments containing no active output sections will have
    /// been filtered, so don't try to index this by our internal segment IDs.
    pub(crate) segments: Vec<SegmentLayout>,
    pub(crate) tls_layout: Option<OutputRecordLayout>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct SegmentLayout {
    pub(crate) id: ProgramSegmentId,
    pub(crate) sizes: OutputRecordLayout,
}

#[derive(Debug)]
pub(crate) struct SymbolResolutions<P: Platform> {
    pub(crate) resolutions: Vec<Option<Resolution<P>>>,
}

impl<P: Platform> SymbolResolutions<P> {
    pub(crate) fn get(&self, symbol_id: SymbolId) -> Option<&Resolution<P>> {
        self.resolutions[symbol_id.as_usize()].as_ref()
    }

    pub(crate) fn raw_values(&self) -> impl Iterator<Item = u64> + '_ {
        self.resolutions
            .iter()
            .map(|r| r.as_ref().map(|res| res.raw_value).unwrap_or(0))
    }
}

pub(crate) enum FileLayout<'data, P: Platform> {
    Prelude(PreludeLayout<'data, P>),
    Object(ObjectLayout<'data, P>),
    Dynamic(DynamicLayout<'data, P>),
    SyntheticSymbols(SyntheticSymbolsLayout<'data, P>),
    Epilogue(EpilogueLayout<P>),
    StubLibrary(StubLibraryLayout<P>),
    NotLoaded,
    LinkerScript(LinkerScriptLayoutState<'data, P>),
}

/// Address information for a symbol.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct Resolution<P: Platform> {
    /// An address or absolute value.
    pub(crate) raw_value: u64,

    pub(crate) dynamic_symbol_index: Option<NonZeroU32>,

    pub(crate) flags: ValueFlags,

    pub(crate) format_specific: P::ResolutionExt,
}

/// Address information for a section.
#[derive(derive_more::Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct SectionResolution {
    #[debug("0x{address:x}")]
    pub(crate) address: u64,
}

impl SectionResolution {
    /// Returns a resolution for a section that we didn't load, or for which we don't have an
    /// address (e.g. string-merge sections).
    pub(crate) fn none() -> SectionResolution {
        SectionResolution { address: u64::MAX }
    }

    pub(crate) fn address(self) -> Option<u64> {
        if self.address == u64::MAX {
            None
        } else {
            Some(self.address)
        }
    }

    /// Converts to a resolution compatible with what's used for symbols.
    pub(crate) fn full_resolution<P: Platform>(self) -> Option<Resolution<P>> {
        let address = self.address()?;
        Some(Resolution {
            raw_value: address,
            dynamic_symbol_index: None,
            flags: ValueFlags::empty(),
            format_specific: Default::default(),
        })
    }
}

pub(crate) enum FileLayoutState<'data, P: Platform> {
    Prelude(PreludeLayoutState<'data, P>),
    Object(ObjectLayoutState<'data, P>),
    Dynamic(DynamicLayoutState<'data, P>),
    StubLibrary(StubLibraryLayoutState<'data, P>),
    NotLoaded(NotLoaded),
    SyntheticSymbols(SyntheticSymbolsLayoutState<'data, P>),
    Epilogue(EpilogueLayoutState<P>),
    LinkerScript(LinkerScriptLayoutState<'data, P>),
}

/// Data that doesn't come from any input files, but needs to be written by the linker.
pub(crate) struct PreludeLayoutState<'data, P: Platform> {
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) internal_symbols: InternalSymbols<'data, P>,
    pub(crate) entry_symbol_id: Option<SymbolId>,
    pub(crate) identity: String,
    pub(crate) header_info: Option<HeaderInfo>,
    pub(crate) dynamic_linker: Option<CString>,
    pub(crate) format_specific: P::PreludeLayoutStateExt,
}

pub(crate) struct SyntheticSymbolsLayoutState<'data, P: Platform> {
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) internal_symbols: InternalSymbols<'data, P>,
}

pub(crate) struct EpilogueLayoutState<P: Platform> {
    pub(crate) format_specific: P::EpilogueLayoutExt,
}

#[derive(Debug)]
pub(crate) struct StubLibraryLayoutState<'data, P: Platform> {
    pub(crate) input: InputRef<'data>,
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) format_specific: P::StubLibraryLayoutStateExt,
}

#[derive(Debug)]
pub(crate) struct StubLibraryLayout<P: Platform> {
    pub(crate) format_specific: P::StubLibraryLayoutExt,
}

#[derive(Debug)]
pub(crate) struct LinkerScriptLayoutState<'data, P: Platform> {
    pub(crate) file_id: FileId,
    pub(crate) input: InputRef<'data>,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) internal_symbols: InternalSymbols<'data, P>,
}

#[derive(Debug)]
pub(crate) struct SyntheticSymbolsLayout<'data, P: Platform> {
    pub(crate) internal_symbols: InternalSymbols<'data, P>,
}

#[derive(Debug)]
pub(crate) struct EpilogueLayout<P: Platform> {
    pub(crate) format_specific: P::EpilogueLayoutExt,
    pub(crate) dynsym_start_index: u32,
}

#[derive(Debug)]
pub(crate) struct ObjectLayout<'data, P: Platform> {
    pub(crate) input: InputRef<'data>,
    pub(crate) file_id: FileId,
    pub(crate) object: &'data P::File<'data>,
    pub(crate) sections: Vec<SectionSlot>,
    pub(crate) relocations: P::RelocationSections,
    pub(crate) section_resolutions: Vec<SectionResolution>,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) section_id_range: SectionIdRange,

    /// SFrame section ranges for this object, relative to the start of the .sframe output section.
    pub(crate) sframe_ranges: Vec<std::ops::Range<usize>>,

    /// Sparse map from section index to relaxation delta details.
    pub(crate) section_relax_deltas: RelaxDeltaMap,

    /// Which ThunkBlock holds primary thunks for this object. Used during relocation writing to
    /// look up the thunk address for out-of-range branch targets.
    pub(crate) thunk_block_id: crate::thunks::ThunkBlockId,

    /// Whether this object is responsible for writing the thunks in its ThunkBlock.
    pub(crate) owns_thunk_block: bool,
}

#[derive(Debug)]
pub(crate) struct PreludeLayout<'data, P: Platform> {
    pub(crate) entry_symbol_id: Option<SymbolId>,
    pub(crate) identity: String,
    pub(crate) header_info: HeaderInfo,
    pub(crate) internal_symbols: InternalSymbols<'data, P>,
    pub(crate) dynamic_linker: Option<CString>,
    pub(crate) format_specific: P::PreludeLayoutExt,
}

#[derive(Debug)]
pub(crate) struct InternalSymbols<'data, P: Platform> {
    pub(crate) symbol_definitions: Vec<InternalSymDefInfo<'data, P>>,
    pub(crate) start_symbol_id: SymbolId,
}

#[derive(Debug)]
pub(crate) struct DynamicLayout<'data, P: Platform> {
    pub(crate) file_id: FileId,
    pub(crate) input: InputRef<'data>,

    /// The name we'll put into the binary to tell the dynamic loader what to load.
    pub(crate) lib_name: &'data [u8],

    pub(crate) symbol_id_range: SymbolIdRange,

    pub(crate) object: &'data P::File<'data>,

    pub(crate) format_specific: P::DynamicLayoutExt<'data>,
}

pub(crate) trait HandlerData {
    fn symbol_id_range(&self) -> SymbolIdRange;

    fn file_id(&self) -> FileId;
}

pub(crate) trait SymbolRequestHandler<'data, P: Platform>:
    std::fmt::Display + HandlerData
{
    fn finalise_symbol_sizes<A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        symbol_flags: &AtomicPerSymbolFlags,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        let symbol_db = resources.symbol_db;

        let _file_span = symbol_db.args.common().trace_span_for_file(self.file_id());
        let symbol_id_range = self.symbol_id_range();

        for (local_index, atomic_flags) in symbol_flags.range(symbol_id_range).iter().enumerate() {
            let symbol_id = symbol_id_range.offset_to_id(local_index);
            if !symbol_db.is_canonical(symbol_id) {
                continue;
            }
            let flags = atomic_flags.get();

            P::finalise_sizes_for_symbol(common, symbol_db, symbol_id, flags)?;

            P::allocate_resolution(
                flags,
                &mut common.mem_sizes,
                symbol_db.output_kind,
                symbol_db.args,
            );

            if symbol_db.args.common().verify_allocation_consistency {
                verify_consistent_allocation_handling::<P, A>(
                    flags,
                    symbol_db.output_kind,
                    symbol_db.args,
                )?;
            }
        }

        Ok(())
    }

    fn load_symbol<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        symbol_id: SymbolId,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        _scope: &Scope<'scope>,
    ) -> Result;
}

impl<'data, P: Platform> HandlerData for ObjectLayoutState<'data, P> {
    fn file_id(&self) -> FileId {
        self.file_id
    }

    fn symbol_id_range(&self) -> SymbolIdRange {
        self.symbol_id_range
    }
}

impl<'data, P: Platform> SymbolRequestHandler<'data, P> for ObjectLayoutState<'data, P> {
    fn load_symbol<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        symbol_id: SymbolId,
        resources: &GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        _scope: &Scope<'scope>,
    ) -> Result {
        debug_assert_bail!(
            resources.symbol_db.is_canonical(symbol_id),
            "Tried to load symbol in a file that doesn't hold the definition: {}",
            resources.symbol_debug(symbol_id)
        );

        let object_symbol_index = self.symbol_id_range.id_to_input(symbol_id);
        let local_symbol = self.object.symbol(object_symbol_index)?;

        if let Some(gc_unit) =
            P::gc_unit_for_symbol(self.object, local_symbol, object_symbol_index)?
        {
            queue
                .local_work
                .push(WorkItem::LoadGcUnit(GcLoadRequest::new(
                    self.file_id,
                    gc_unit,
                )));
        } else if let Some(common_symbol) = local_symbol.as_common() {
            common.allocate(common_symbol.part_id, common_symbol.size);
        }

        Ok(())
    }
}

impl<'data, P: Platform> HandlerData for DynamicLayoutState<'data, P> {
    fn symbol_id_range(&self) -> SymbolIdRange {
        self.symbol_id_range
    }

    fn file_id(&self) -> FileId {
        self.file_id
    }
}

impl<'data, P: Platform> SymbolRequestHandler<'data, P> for DynamicLayoutState<'data, P> {
    fn load_symbol<'scope, A: Arch<Platform = P>>(
        &mut self,
        _common: &mut CommonGroupState<'data, P>,
        symbol_id: SymbolId,
        resources: &GraphResources<'data, 'scope, P>,
        _queue: &mut LocalWorkQueue<P>,
        _scope: &Scope<'scope>,
    ) -> Result {
        let local_index = object::SymbolIndex(symbol_id.to_offset(self.symbol_id_range()));
        self.object.dynamic_symbol_used(local_index, self)?;

        // Check for arch-specific VARIANT_PCS flags.
        if A::is_symbol_variant_pcs(self.object, local_index) {
            resources
                .has_variant_pcs
                .store(true, atomic::Ordering::Relaxed);
        }

        Ok(())
    }
}

impl<P: Platform> HandlerData for PreludeLayoutState<'_, P> {
    fn file_id(&self) -> FileId {
        self.file_id
    }

    fn symbol_id_range(&self) -> SymbolIdRange {
        self.symbol_id_range
    }
}

impl<'data, P: Platform> SymbolRequestHandler<'data, P> for PreludeLayoutState<'data, P> {
    fn load_symbol<'scope, A: Arch<Platform = P>>(
        &mut self,
        _common: &mut CommonGroupState<'data, P>,
        _symbol_id: SymbolId,
        _resources: &GraphResources<'data, 'scope, P>,
        _queue: &mut LocalWorkQueue<P>,
        _scope: &Scope<'scope>,
    ) -> Result {
        Ok(())
    }
}

impl<P: Platform> HandlerData for LinkerScriptLayoutState<'_, P> {
    fn symbol_id_range(&self) -> SymbolIdRange {
        self.symbol_id_range
    }

    fn file_id(&self) -> FileId {
        self.file_id
    }
}

impl<'data, P: Platform> SymbolRequestHandler<'data, P> for LinkerScriptLayoutState<'data, P> {
    fn load_symbol<'scope, A: Arch<Platform = P>>(
        &mut self,
        _common: &mut CommonGroupState<'data, P>,
        _symbol_id: SymbolId,
        _resources: &GraphResources<'data, 'scope, P>,
        _queue: &mut LocalWorkQueue<P>,
        _scope: &Scope<'scope>,
    ) -> Result {
        Ok(())
    }
}

impl<P: Platform> HandlerData for StubLibraryLayoutState<'_, P> {
    fn file_id(&self) -> FileId {
        self.file_id
    }

    fn symbol_id_range(&self) -> SymbolIdRange {
        self.symbol_id_range
    }
}

impl<'data, P: Platform> SymbolRequestHandler<'data, P> for StubLibraryLayoutState<'data, P> {
    fn load_symbol<'scope, A: Arch<Platform = P>>(
        &mut self,
        _common: &mut CommonGroupState<'data, P>,
        _symbol_id: SymbolId,
        _resources: &GraphResources<'data, 'scope, P>,
        _queue: &mut LocalWorkQueue<P>,
        _scope: &Scope<'scope>,
    ) -> Result {
        Ok(())
    }
}

impl<P: Platform> HandlerData for SyntheticSymbolsLayoutState<'_, P> {
    fn file_id(&self) -> FileId {
        self.file_id
    }

    fn symbol_id_range(&self) -> SymbolIdRange {
        self.symbol_id_range
    }
}

impl<'data, P: Platform> SymbolRequestHandler<'data, P> for SyntheticSymbolsLayoutState<'data, P> {
    fn load_symbol<'scope, A: Arch<Platform = P>>(
        &mut self,
        _common: &mut CommonGroupState<'data, P>,
        symbol_id: SymbolId,
        resources: &'scope GraphResources<'data, 'scope, P>,
        _queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        let def_info =
            &self.internal_symbols.symbol_definitions[self.symbol_id_range.id_to_offset(symbol_id)];

        if let Some(output_section_id) = def_info.section_id() {
            // We've gotten a request to load a __start_ / __stop_ symbol, sent requests to load all
            // sections that would go into that section.
            let sections = resources.start_stop_sections.get(output_section_id);
            while let Some(request) = sections.pop() {
                resources.send_work::<A>(
                    request.file_id,
                    WorkItem::LoadGcUnit(request),
                    resources,
                    scope,
                );
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CommonGroupState<'data, P: Platform> {
    pub(crate) mem_sizes: OutputSectionPartMap<u64>,

    pub(crate) section_attributes: HashMap<OutputSectionId, P::SectionAttributes>,

    /// Dynamic symbols that need to be defined. Because of the ordering requirements for symbol
    /// hashes, these get defined by the epilogue. The object on which a particular dynamic symbol
    /// is stored is non-deterministic and is whichever object first requested export of that
    /// symbol. That's OK though because the epilogue will sort all dynamic symbols.
    pub(crate) dynamic_symbol_definitions: Vec<DynamicSymbolDefinition<'data, P>>,

    pub(crate) format_specific: P::CommonGroupStateExt,
}

impl<'data, P: Platform> CommonGroupState<'data, P> {
    pub(crate) fn new(output_sections: &OutputSections<P>) -> Self {
        Self {
            mem_sizes: output_sections.new_part_map(),
            section_attributes: Default::default(),
            dynamic_symbol_definitions: Default::default(),
            format_specific: Default::default(),
        }
    }

    pub(crate) fn validate_sizes(&self) -> Result {
        P::validate_sizes(&self.mem_sizes)
    }

    pub(crate) fn finalise_layout(
        &self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        section_layouts: &OutputSectionMap<OutputRecordLayout>,
    ) -> u32 {
        let mut strtab_offset_start = 0;
        if let Some((strtab_section_id, strtab_part_id)) =
            P::STRTAB_SECTION_ID.and_then(|section_id| {
                P::single_part_id(section_id).map(|part_id| (section_id, part_id))
            })
        {
            let offset = memory_offsets.get_mut(strtab_part_id);
            strtab_offset_start = (*offset - section_layouts.get(strtab_section_id).mem_offset)
                .try_into()
                .expect("Symbol string table overflowed 32 bits");
            *offset += self.mem_sizes.get(strtab_part_id);
        }

        for section_id in [
            P::SYMTAB_LOCAL_SECTION_ID,
            P::SYMTAB_GLOBAL_SECTION_ID,
            P::SYMTAB_SHNDX_LOCAL_SECTION_ID,
            P::SYMTAB_SHNDX_GLOBAL_SECTION_ID,
            P::GDB_INDEX_SECTION_ID,
        ]
        .into_iter()
        .flatten()
        {
            if let Some(part_id) = P::single_part_id(section_id) {
                memory_offsets.increment(part_id, self.mem_sizes.get(part_id));
            }
        }

        strtab_offset_start
    }

    pub(crate) fn allocate(&mut self, part_id: PartId, size: u64) {
        self.mem_sizes.increment(part_id, size);
    }

    pub(crate) fn store_section_attributes(&mut self, part_id: PartId, header: &P::SectionHeader) {
        let new_attributes = P::section_attributes(header);

        match self
            .section_attributes
            .entry(part_id.output_section_id::<P>())
        {
            hashbrown::hash_map::Entry::Occupied(occupied_entry) => {
                occupied_entry.into_mut().merge(new_attributes);
            }
            hashbrown::hash_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(new_attributes);
            }
        }
    }
}

pub(crate) struct ObjectLayoutState<'data, P: Platform> {
    pub(crate) input: InputRef<'data>,
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) section_id_range: SectionIdRange,
    pub(crate) object: &'data P::File<'data>,

    /// Command-line section concatenation order. Plugin codegen shares the first LTO input's
    /// position (#1935).
    pub(crate) link_order: u32,

    /// Info about each of our sections. Indexed the same as the sections in the input object.
    pub(crate) sections: Vec<SectionSlot>,

    /// Mapping from sections to their corresponding relocation section.
    pub(crate) relocations: P::RelocationSections,

    pub(crate) format_specific: P::ObjectLayoutStateExt<'data>,

    /// Sparse map from section index to relaxation delta details, built during `finalise_sizes`
    /// and later transferred to `ObjectLayout`.
    pub(crate) section_relax_deltas: RelaxDeltaMap,

    pub(crate) script_sorted_sections: Vec<ScriptSortedSectionDetail>,

    /// Which ThunkBlock handles primary-part thunks for this object.
    pub(crate) thunk_block_id: ThunkBlockId,

    /// Whether this object is responsible for writing the thunk block.
    pub(crate) owns_thunk_block: bool,

    /// Total bytes of primary-function-part sections that survived GC. Used to help determine
    /// distances for range-extension thunks.
    pub(crate) post_gc_primary_bytes: u64,
}

#[derive(Debug, Default)]
pub(crate) struct LocalWorkQueue<P: Platform> {
    /// The index of the worker that owns this queue.
    pub(crate) index: usize,

    /// Work that needs to be processed by the worker that owns this queue.
    pub(crate) local_work: Vec<WorkItem<P>>,
}

pub(crate) struct DynamicLayoutState<'data, P: Platform> {
    pub(crate) object: &'data P::File<'data>,
    pub(crate) input: InputRef<'data>,
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) lib_name: &'data [u8],

    pub(crate) format_specific: P::DynamicLayoutStateExt<'data>,
}

#[derive(derive_more::Debug, Clone, Copy)]
pub(crate) struct DynamicSymbolDefinition<'data, P: Platform> {
    pub(crate) symbol_id: SymbolId,
    #[debug("{:?}", String::from_utf8_lossy(name))]
    pub(crate) name: &'data [u8],
    pub(crate) format_specific: P::DynamicSymbolDefinitionExt,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Section {
    /// Size in the output. This starts as the input section size, then may be reduced by
    /// relaxation-induced byte deletions during `scan_relaxations`.
    pub(crate) size: u64,
    pub(crate) alignment: Alignment,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SortedSection {
    pub(crate) address: u64,
    pub(crate) section: Section,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SectionGroupOrder {
    Prelude,
    Object(u32),
    Other,
    Epilogue,
}

pub(crate) fn section_group_order<P: Platform>(files: &[FileLayoutState<P>]) -> SectionGroupOrder {
    let mut saw_object: Option<u32> = None;
    for file in files {
        match file {
            FileLayoutState::Prelude(_) => return SectionGroupOrder::Prelude,
            FileLayoutState::Epilogue(_) => return SectionGroupOrder::Epilogue,
            FileLayoutState::Object(obj) => {
                saw_object = Some(match saw_object {
                    Some(existing) => existing.min(obj.link_order),
                    None => obj.link_order,
                });
            }
            _ => {}
        }
    }
    match saw_object {
        Some(link_order) => SectionGroupOrder::Object(link_order),
        None => SectionGroupOrder::Other,
    }
}

#[derive(Debug)]
pub(crate) struct GroupLayout<'data, P: Platform> {
    pub(crate) files: Vec<FileLayout<'data, P>>,

    /// The offset in .dynstr at which we'll start writing.
    pub(crate) dynstr_start_offset: u32,

    /// The offset in .strtab at which we'll start writing.
    pub(crate) strtab_start_offset: u32,

    pub(crate) symtab_local_start_index: u32,
    pub(crate) symtab_global_start_index: u32,

    pub(crate) mem_sizes: OutputSectionPartMap<u64>,
    pub(crate) file_sizes: OutputSectionPartMap<usize>,

    pub(crate) format_specific: P::GroupLayoutExt,

    pub(crate) section_group_order: SectionGroupOrder,
}

#[derive(Debug)]
pub(crate) struct GroupState<'data, P: Platform> {
    pub(crate) queue: LocalWorkQueue<P>,
    pub(crate) files: Vec<FileLayoutState<'data, P>>,
    pub(crate) common: CommonGroupState<'data, P>,
    pub(crate) num_symbols: usize,
    pub(crate) section_group_order: SectionGroupOrder,
}

/// The sizes and positions of either a segment or an output section. Note, we use usize for file
/// offsets and sizes, since we mmap our output file, so we're frequently working with in-memory
/// slices. This means that if we were linking on a 32 bit system that we'd be limited to file
/// offsets that were 32 bits. This isn't a loss though, since we couldn't mmap an output file where
/// that would be a problem on a 32 bit system.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputRecordLayout {
    pub(crate) file_size: usize,
    pub(crate) mem_size: u64,
    pub(crate) alignment: Alignment,
    pub(crate) file_offset: usize,
    pub(crate) mem_offset: u64,
    pub(crate) lma_offset: u64,
}

pub(crate) struct GraphResources<'data, 'scope, P: Platform> {
    pub(crate) symbol_db: &'scope SymbolDb<'data, P>,

    pub(crate) output_sections: &'scope OutputSections<'data, P>,

    pub(crate) worker_slots: Vec<Mutex<WorkerSlot<'data, P>>>,

    pub(crate) errors: Mutex<Vec<Error>>,

    pub(crate) per_symbol_flags: &'scope AtomicPerSymbolFlags<'scope>,

    /// Sections that we'll keep, even if their total size is zero.
    pub(crate) must_keep_sections: OutputSectionMap<AtomicBool>,

    pub(crate) has_static_tls: AtomicBool,

    pub(crate) has_variant_pcs: AtomicBool,

    pub(crate) thunk_layout_builder: Option<crate::thunks::ThunkLayoutBuilder>,

    /// For each OutputSectionId, this tracks a list of sections that should be loaded if that
    /// section gets referenced. The sections here will only be those that are eligible for having
    /// __start_ / __stop_ symbols. i.e. sections that don't start their names with a ".".
    pub(crate) start_stop_sections: OutputSectionMap<SegQueue<GcLoadRequest<P>>>,

    /// The number of groups that haven't yet completed activation.
    pub(crate) activations_remaining: AtomicUsize,

    /// Groups that cannot be processed until all groups have completed activation.
    pub(crate) delay_processing: ArrayQueue<GroupState<'data, P>>,

    pub(crate) layout_resources_ext: P::LayoutResourcesExt<'data>,
}

pub(crate) struct FinaliseLayoutResources<'scope, 'data, P: Platform> {
    pub(crate) symbol_db: &'scope SymbolDb<'data, P>,
    pub(crate) per_symbol_flags: &'scope PerSymbolFlags,
    pub(crate) output_sections: &'scope OutputSections<'data, P>,
    pub(crate) output_order: &'scope OutputOrder<'data>,
    pub(crate) section_layouts: &'scope OutputSectionMap<OutputRecordLayout>,
    pub(crate) merged_string_start_addresses: &'scope MergedStringStartAddresses,
    pub(crate) merged_strings: &'scope OutputSectionMap<MergedStringsSection<'data>>,
    pub(crate) dynamic_symbol_definitions: &'scope Vec<DynamicSymbolDefinition<'data, P>>,
    pub(crate) segment_layouts: &'scope SegmentLayouts,
    pub(crate) program_segments: &'scope ProgramSegments<P::ProgramSegmentDef>,
    pub(crate) script_sorted_sections: &'scope [InputSortedSection],
    pub(crate) format_specific: &'scope P::FinaliseSizesExt<'data>,

    pub(crate) thunk_blocks: &'scope [crate::thunks::ThunkBlock],

    /// Per-thunk-block addresses-maps. We could store this on ObjectLayoutState, but only a small
    /// fraction of the input objects will be thunk-block owners, so it'd seem wasteful. Instead we
    /// put it here and wrap each map in a mutex. Since each map is only written by its owner, each
    /// mutex should only ever get locked once during its lifetime.
    pub(crate) thunk_block_addresses: &'scope Vec<Mutex<BTreeMap<SymbolId, u64>>>,
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum WorkItem<P: Platform> {
    /// The symbol's resolution flags have been made non-empty. The object that owns the symbol
    /// should perform any additional actions required, e.g. load the section that contains the
    /// symbol and process any relocations for that section.
    LoadGlobalSymbol(SymbolId),

    /// A direct reference to a dynamic symbol has been encountered. The symbol should be defined in
    /// BSS with a copy relocation.
    CopyRelocateSymbol(SymbolId),

    /// A request to load a particular GC unit.
    LoadGcUnit(GcLoadRequest<P>),

    /// Requests that the specified symbol be exported as a dynamic symbol. Will be ignored if the
    /// object that defines the symbol is not loaded or is itself a shared object.
    ExportDynamic(SymbolId),
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct GcLoadRequest<P: Platform> {
    pub(crate) file_id: FileId,

    pub(crate) gc_unit: P::GcUnit,
}

impl<P: Platform> WorkItem<P> {
    pub(crate) fn file_id(self, symbol_db: &SymbolDb<P>) -> FileId {
        match self {
            WorkItem::LoadGlobalSymbol(s) | WorkItem::CopyRelocateSymbol(s) => {
                symbol_db.file_id_for_symbol(s)
            }
            WorkItem::LoadGcUnit(s) => s.file_id,
            WorkItem::ExportDynamic(symbol_id) => symbol_db.file_id_for_symbol(symbol_id),
        }
    }
}

pub(crate) struct MemoryRegion {
    pub(crate) origin: u64,
    pub(crate) length: u64,
    pub(crate) used: u64,
    pub(crate) used_lma: u64,
    pub(crate) flags: Option<crate::linker_script::MemoryFlags>,
}

impl<'data, P: Platform> Layout<'data, P> {
    pub(crate) fn prelude(&self) -> &PreludeLayout<'data, P> {
        let Some(FileLayout::Prelude(i)) = self.group_layouts.first().and_then(|g| g.files.first())
        else {
            panic!("Prelude layout not found at expected offset");
        };
        i
    }

    pub(crate) fn args(&self) -> &'data P::Args {
        self.symbol_db.args
    }

    /// Loaded allocatable section sizes per input object, for incremental layout matching.
    pub(crate) fn incremental_object_records(&self) -> Vec<(FileId, PathBuf, Vec<u64>)> {
        let mut records = Vec::new();
        for group in &self.group_layouts {
            for file in &group.files {
                let FileLayout::Object(obj) = file else {
                    continue;
                };
                let sizes = obj
                    .sections
                    .iter()
                    .filter_map(|slot| match slot {
                        SectionSlot::Loaded(sec) => Some(sec.size),
                        _ => None,
                    })
                    .collect();
                records.push((obj.file_id, obj.input.file.filename.to_path_buf(), sizes));
            }
        }
        records
    }

    pub(crate) fn skip_incremental_payload(&self, file_id: FileId) -> bool {
        self.incremental_skip_payloads.contains(&file_id)
    }

    pub(crate) fn record_reverse_reloc(
        &self,
        symbol_id: SymbolId,
        file_offset: u64,
        place: u64,
        addend: i64,
        r_type: u32,
        file_id: FileId,
    ) {
        if !self.args().common().incremental {
            return;
        }
        let defined = self.symbol_db.definition(symbol_id);
        self.incremental_reverse_relocs.lock().unwrap().push(
            defined.as_usize(),
            file_offset,
            place,
            addend,
            r_type,
            file_id.as_u32(),
        );
    }

    pub(crate) fn take_reverse_relocs(&self) -> crate::incremental::ReverseRelocIndex {
        replace(
            &mut *self.incremental_reverse_relocs.lock().unwrap(),
            crate::incremental::ReverseRelocIndex::new(0),
        )
    }

    pub(crate) fn symbol_debug<'layout>(
        &'layout self,
        symbol_id: SymbolId,
    ) -> SymbolDebug<'layout, 'data, P> {
        self.symbol_db
            .symbol_debug(&self.per_symbol_flags, symbol_id)
    }

    #[inline(always)]
    pub(crate) fn merged_symbol_resolution(&self, symbol_id: SymbolId) -> Option<Resolution<P>> {
        self.local_symbol_resolution(self.symbol_db.definition(symbol_id))
            .copied()
            .map(|mut res| {
                res.flags.merge(
                    self.symbol_db
                        .flags_for_symbol(&self.per_symbol_flags, symbol_id),
                );
                res
            })
    }

    pub(crate) fn local_symbol_resolution(&self, symbol_id: SymbolId) -> Option<&Resolution<P>> {
        self.symbol_resolutions.get(symbol_id)
    }

    pub(crate) fn resolutions_in_range(
        &self,
        range: SymbolIdRange,
    ) -> impl Iterator<Item = (SymbolId, Option<&Resolution<P>>)> {
        self.symbol_resolutions.resolutions[range.as_usize()]
            .iter()
            .enumerate()
            .map(move |(i, res)| (range.offset_to_id(i), res.as_ref()))
    }

    pub(crate) fn resolved_entry_symbol_address(&self) -> Result<Option<u64>> {
        let Some(symbol_id) = self.prelude().entry_symbol_id else {
            return Ok(None);
        };
        let resolution = self.local_symbol_resolution(symbol_id).with_context(|| {
            format!(
                "Entry point symbol was defined, but didn't get loaded. {}",
                self.symbol_debug(symbol_id)
            )
        })?;

        if !resolution.flags().has_link_time_address() && !resolution.flags().is_absolute() {
            bail!(
                "Entry point must be an address or absolute value. {}",
                self.symbol_debug(symbol_id)
            );
        }

        Ok(Some(resolution.value()))
    }

    pub(crate) fn tls_start_address(&self) -> u64 {
        // If we don't have a TLS segment then the value we return won't really matter.
        self.segment_layouts
            .tls_layout
            .as_ref()
            .map_or(0, |seg| seg.mem_offset)
    }

    pub(crate) fn tls_start_address_aligned(&self) -> u64 {
        self.segment_layouts
            .tls_layout
            .as_ref()
            .map_or(0, |seg| seg.alignment.align_down(seg.mem_offset))
    }

    /// Returns the memory address of the end of the TLS segment including any padding required to
    /// make sure that the TCB will be usize-aligned.
    pub(crate) fn tls_end_address(&self) -> u64 {
        self.segment_layouts.tls_layout.as_ref().map_or(0, |seg| {
            seg.alignment.align_up(seg.mem_offset + seg.mem_size)
        })
    }

    /// Returns the memory address of the start of the TLS segment used by the AArch64.
    pub(crate) fn tls_start_address_aarch64(&self) -> u64 {
        self.segment_layouts.tls_layout.as_ref().map_or(0, |seg| {
            seg.alignment
                .align_down(seg.mem_offset - linker_utils::aarch64::TLS_TCB_SIZE)
        })
    }

    pub(crate) fn layout_data(&self) -> linker_layout::Layout {
        let thunk_count = self.thunk_count();

        let files = self
            .group_layouts
            .iter()
            .flat_map(|group| {
                group.files.iter().filter_map(|file| match file {
                    FileLayout::Object(obj) => Some(linker_layout::InputFile {
                        path: obj.input.file.filename.to_owned(),
                        archive_entry: obj.input.entry.as_ref().map(|e| {
                            linker_layout::ArchiveEntryInfo {
                                range: e.byte_range(),
                                identifier: e.identifier.as_slice().to_owned(),
                            }
                        }),
                        sections: obj
                            .section_resolutions
                            .iter()
                            .enumerate()
                            .zip(obj.object.section_iter())
                            .zip(&obj.sections)
                            .map(|(((idx, res), section), section_slot)| {
                                let part_id = obj.section_part_id(
                                    object::SectionIndex(idx),
                                    &self.symbol_db.section_part_ids,
                                );
                                let primary_id = self
                                    .output_sections
                                    .primary_output_section(part_id.output_section_id::<P>());
                                let output_flags = self.output_sections.section_flags(primary_id);

                                (matches!(section_slot, SectionSlot::Loaded(..))
                                    && output_flags.is_alloc()
                                    && obj.object.section_size(section).is_ok_and(|s| s > 0))
                                .then(|| {
                                    let address = res.address;
                                    let size = match section_slot {
                                        SectionSlot::Loaded(sec) => sec.size,
                                        _ => obj.object.section_size(section).unwrap(),
                                    };
                                    linker_layout::Section {
                                        mem_range: address..(address + size),
                                    }
                                })
                            })
                            .collect(),
                        temporary: obj.input.file.modifiers.temporary,
                    }),
                    _ => None,
                })
            })
            .collect();

        linker_layout::Layout {
            files,
            metrics: linker_layout::Metrics { thunk_count },
        }
    }

    pub(crate) fn thunk_count(&self) -> u64 {
        self.thunk_block_addresses
            .iter()
            .map(|m| m.len() as u64)
            .sum()
    }

    pub(crate) fn flags_for_symbol(&self, symbol_id: SymbolId) -> ValueFlags {
        self.symbol_db
            .flags_for_symbol(&self.per_symbol_flags, symbol_id)
    }

    pub(crate) fn file_layout(&self, file_id: FileId) -> &FileLayout<'data, P> {
        let group_layout = &self.group_layouts[file_id.group()];
        &group_layout.files[file_id.file()]
    }

    /// Returns the base address of the global offset table. This needs to be consistent with the
    /// symbol `_GLOBAL_OFFSET_TABLE_`.
    pub(crate) fn got_base(&self) -> u64 {
        let got_layout = self
            .section_layouts
            .get(P::GOT_SECTION_ID.expect("platform has no GOT section"));
        got_layout.mem_offset
    }

    /// Returns whether we're going to output the .gnu.version section.
    pub(crate) fn gnu_version_enabled(&self) -> bool {
        P::GNU_VERSION_SECTION_ID.is_some_and(|section_id| {
            self.section_part_layouts
                .get(section_id.base_part_id::<P>())
                .file_size
                > 0
        })
    }
}

#[derive(Default)]
pub(crate) struct WorkerSlot<'data, P: Platform> {
    pub(crate) work: Vec<WorkItem<P>>,
    pub(crate) worker: Option<GroupState<'data, P>>,
}

#[derive(Debug)]
pub(crate) struct GcOutputs<'data, P: Platform> {
    pub(crate) group_states: Vec<GroupState<'data, P>>,
    pub(crate) must_keep_sections: OutputSectionMap<bool>,
    pub(crate) has_static_tls: bool,
    pub(crate) has_variant_pcs: bool,
    pub(crate) thunk_layout_builder: Option<ThunkLayoutBuilder>,
}

pub(crate) struct GroupActivationInputs<'data, P: Platform> {
    pub(crate) resolved: ResolvedGroup<'data, P>,
    pub(crate) num_symbols: usize,
    pub(crate) group_index: usize,
}

impl<'data, P: Platform> GroupActivationInputs<'data, P> {
    pub(crate) fn activate_group<'scope, A: Arch<Platform = P>>(
        self,
        resources: &'scope GraphResources<'data, '_, P>,
        scope: &Scope<'scope>,
    ) {
        let GroupActivationInputs {
            resolved,
            num_symbols,
            group_index,
        } = self;

        let files = resolved
            .files
            .into_iter()
            .map(|file| file.create_layout_state(resources.symbol_db.args))
            .collect();

        let mut group = GroupState {
            queue: LocalWorkQueue::new(group_index),
            num_symbols,
            files,
            common: CommonGroupState::new(resources.output_sections),
            section_group_order: SectionGroupOrder::Other,
        };
        group.section_group_order = section_group_order(&group.files);

        let mut should_delay_processing = false;

        for file in &mut group.files {
            let r = activate::<A>(&mut group.common, file, &mut group.queue, resources, scope)
                .with_context(|| format!("Failed to activate {file}"));

            // SyntheticSymbols can't be processed until all groups have completed activation, since
            // it can read from `start_stop_sections` which gets populated by other objects during
            // activation.
            should_delay_processing |= matches!(file, FileLayoutState::SyntheticSymbols(_));

            if let Err(error) = r {
                resources.errors.lock().unwrap().push(error);
            }
        }

        if should_delay_processing {
            resources.delay_processing.push(group).unwrap();
        } else {
            group.do_pending_work::<A>(resources, scope);
        }

        let remaining = resources
            .activations_remaining
            .fetch_sub(1, atomic::Ordering::Relaxed)
            - 1;

        if remaining == 0 {
            while let Some(group) = resources.delay_processing.pop() {
                group.do_pending_work::<A>(resources, scope);
            }
        }
    }
}

impl<'data, P: Platform> GroupState<'data, P> {
    /// Does work until there's nothing left in the queue, then returns our worker to its slot and
    /// shuts down.
    pub(crate) fn do_pending_work<'scope, A: Arch<Platform = P>>(
        mut self,
        resources: &'scope GraphResources<'data, '_, P>,
        scope: &Scope<'scope>,
    ) {
        loop {
            while let Some(work_item) = self.queue.local_work.pop() {
                let file_id = work_item.file_id(resources.symbol_db);
                let file = &mut self.files[file_id.file()];
                if let Err(error) = file.do_work::<A>(
                    &mut self.common,
                    work_item,
                    resources,
                    &mut self.queue,
                    scope,
                ) {
                    resources.report_error(error);
                    return;
                }
            }
            {
                let mut slot = resources.worker_slots[self.queue.index].lock().unwrap();
                if slot.work.is_empty() {
                    slot.worker = Some(self);
                    return;
                }
                swap(&mut slot.work, &mut self.queue.local_work);
            };
        }
    }

    pub(crate) fn finalise_sizes<A: Arch<Platform = P>>(
        &mut self,
        per_symbol_flags: &AtomicPerSymbolFlags,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        for file_state in &mut self.files {
            file_state.finalise_sizes::<A>(&mut self.common, per_symbol_flags, resources)?;
        }

        self.common.validate_sizes()?;
        Ok(())
    }

    pub(crate) fn finalise_layout(
        self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut sharded_vec_writer::Shard<Option<Resolution<P>>>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result<GroupLayout<'data, P>> {
        let format_specific = P::finalise_group_layout(memory_offsets);
        let files = self
            .files
            .into_iter()
            .map(|file| file.finalise_layout(memory_offsets, resolutions_out, resources))
            .collect::<Result<Vec<_>>>()?;

        let entry_size = size_of::<P::SymtabEntry>() as u64;
        let symtab_local_start_index = P::SYMTAB_LOCAL_SECTION_ID
            .and_then(|section_id| {
                P::single_part_id(section_id).map(|part_id| (section_id, part_id))
            })
            .map_or(0, |(section_id, part_id)| {
                ((memory_offsets.get(part_id)
                    - resources.section_layouts.get(section_id).mem_offset)
                    / entry_size) as u32
            });
        let symtab_global_start_index = P::SYMTAB_GLOBAL_SECTION_ID
            .and_then(|section_id| {
                P::single_part_id(section_id).map(|part_id| (section_id, part_id))
            })
            .map_or(0, |(section_id, part_id)| {
                ((memory_offsets.get(part_id)
                    - resources.section_layouts.get(section_id).mem_offset)
                    / entry_size) as u32
            });

        let strtab_start_offset = self
            .common
            .finalise_layout(memory_offsets, resources.section_layouts);
        let dynstr_start_offset = P::DYNSTR_SECTION_ID
            .and_then(|section_id| {
                P::single_part_id(section_id).map(|part_id| (section_id, part_id))
            })
            .map_or(0, |(section_id, part_id)| {
                let start = (memory_offsets.get(part_id)
                    - resources.section_layouts.get(section_id).mem_offset)
                    as u32;
                memory_offsets.increment(part_id, self.common.mem_sizes.get(part_id));
                start
            });

        Ok(GroupLayout {
            files,
            strtab_start_offset,
            dynstr_start_offset,
            symtab_local_start_index,
            symtab_global_start_index,
            file_sizes: compute_file_sizes(&self.common.mem_sizes, resources.output_sections),
            mem_sizes: self.common.mem_sizes,
            format_specific,
            section_group_order: self.section_group_order,
        })
    }
}

impl<P: Platform> LocalWorkQueue<P> {
    #[inline(always)]
    pub(crate) fn send_work<'data, 'scope, A: Arch<Platform = P>>(
        &mut self,
        resources: &'scope GraphResources<'data, '_, A::Platform>,
        file_id: FileId,
        work: WorkItem<P>,
        scope: &Scope<'scope>,
    ) {
        if file_id.group() == self.index {
            self.local_work.push(work);
        } else {
            resources.send_work::<A>(file_id, work, resources, scope);
        }
    }

    pub(crate) fn new(index: usize) -> LocalWorkQueue<P> {
        Self {
            index,
            local_work: Default::default(),
        }
    }

    #[inline(always)]
    pub(crate) fn send_symbol_request<'data, 'scope, A: Arch<Platform = P>>(
        &mut self,
        symbol_id: SymbolId,
        resources: &'scope GraphResources<'data, '_, A::Platform>,
        scope: &Scope<'scope>,
    ) {
        debug_assert!(resources.symbol_db.is_canonical(symbol_id));
        let symbol_file_id = resources.symbol_db.file_id_for_symbol(symbol_id);
        self.send_work::<A>(
            resources,
            symbol_file_id,
            WorkItem::LoadGlobalSymbol(symbol_id),
            scope,
        );
    }

    pub(crate) fn send_gc_unit_request<'data, 'scope, A: Arch<Platform = P>>(
        &mut self,
        file_id: FileId,
        gc_unit: P::GcUnit,
        resources: &'scope GraphResources<'data, '_, A::Platform>,
        scope: &Scope<'scope>,
    ) {
        self.send_work::<A>(
            resources,
            file_id,
            WorkItem::LoadGcUnit(GcLoadRequest::new(file_id, gc_unit)),
            scope,
        );
    }

    pub(crate) fn send_copy_relocation_request<'data, 'scope, A: Arch<Platform = P>>(
        &mut self,
        symbol_id: SymbolId,
        resources: &'scope GraphResources<'data, '_, A::Platform>,
        scope: &Scope<'scope>,
    ) {
        debug_assert!(resources.symbol_db.is_canonical(symbol_id));
        let symbol_file_id = resources.symbol_db.file_id_for_symbol(symbol_id);
        self.send_work::<A>(
            resources,
            symbol_file_id,
            WorkItem::CopyRelocateSymbol(symbol_id),
            scope,
        );
    }
}

impl<'data, P: Platform> GraphResources<'data, '_, P> {
    pub(crate) fn report_error(&self, error: Error) {
        self.errors.lock().unwrap().push(error);
    }

    /// Sends all work in `work` to the worker for `file_id`. Leaves `work` empty so that it can be
    /// reused.
    #[inline(always)]
    pub(crate) fn send_work<'scope, A: Arch<Platform = P>>(
        &self,
        file_id: FileId,
        work: WorkItem<P>,
        resources: &'scope GraphResources<'data, '_, P>,
        scope: &Scope<'scope>,
    ) {
        let worker;
        {
            let mut slot = self.worker_slots[file_id.group()].lock().unwrap();
            worker = slot.worker.take();
            slot.work.push(work);
        };
        if let Some(worker) = worker {
            scope.spawn(|scope| {
                verbose_timing_phase!("Work with object");
                worker.do_pending_work::<A>(resources, scope);
            });
        }
    }

    pub(crate) fn local_flags_for_symbol(&self, symbol_id: SymbolId) -> ValueFlags {
        self.per_symbol_flags.flags_for_symbol(symbol_id)
    }

    pub(crate) fn symbol_debug<'a>(&'a self, symbol_id: SymbolId) -> SymbolDebug<'a, 'data, P> {
        self.symbol_db
            .symbol_debug(self.per_symbol_flags, symbol_id)
    }

    pub(crate) fn keep_section(&self, section_id: OutputSectionId) {
        let keep = self.must_keep_sections.get(section_id);

        // We only write after reading and determining that we need to write. This likely makes the
        // case where we do write slower, but the case where we don't write faster and also avoids
        // gaining exclusive access to the cache line unless necessary. This has a small but
        // measurable performance effect.
        if !keep.load(atomic::Ordering::Relaxed) {
            keep.store(true, atomic::Ordering::Relaxed);
        }
    }
}

impl<'data, P: Platform> FileLayoutState<'data, P> {
    pub(crate) fn finalise_sizes<A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        per_symbol_flags: &AtomicPerSymbolFlags,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        match self {
            FileLayoutState::Object(s) => {
                s.finalise_sizes(common, per_symbol_flags, resources)?;
                s.finalise_symbol_sizes::<A>(common, per_symbol_flags, resources)?;
            }
            FileLayoutState::Dynamic(s) => {
                s.finalise_sizes(common)?;
                s.finalise_symbol_sizes::<A>(common, per_symbol_flags, resources)?;
            }
            FileLayoutState::Prelude(s) => {
                PreludeLayoutState::finalise_sizes(common, resources.merged_strings);
                s.finalise_symbol_sizes::<A>(common, per_symbol_flags, resources)?;
            }
            FileLayoutState::SyntheticSymbols(s) => {
                s.finalise_sizes(common, per_symbol_flags, resources)?;
                s.finalise_symbol_sizes::<A>(common, per_symbol_flags, resources)?;
            }
            FileLayoutState::Epilogue(s) => {
                s.finalise_sizes(common, resources);
            }
            FileLayoutState::LinkerScript(s) => {
                s.finalise_sizes(common, per_symbol_flags, resources)?;
                s.finalise_symbol_sizes::<A>(common, per_symbol_flags, resources)?;
            }
            FileLayoutState::StubLibrary(s) => {
                s.finalise_symbol_sizes::<A>(common, per_symbol_flags, resources)?;
            }
            FileLayoutState::NotLoaded(_) => {}
        }

        P::finalise_sizes_all(&mut common.mem_sizes, resources.symbol_db);

        Ok(())
    }

    pub(crate) fn do_work<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        work_item: WorkItem<P>,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        match work_item {
            WorkItem::LoadGlobalSymbol(symbol_id) => self
                .handle_symbol_request::<A>(common, symbol_id, resources, queue, scope)
                .with_context(|| {
                    format!(
                        "Failed to load {} from {self}",
                        resources.symbol_debug(symbol_id),
                    )
                }),
            WorkItem::CopyRelocateSymbol(symbol_id) => match self {
                FileLayoutState::Dynamic(state) => {
                    P::copy_relocate_symbol(state, symbol_id, resources)
                }

                _ => {
                    bail!(
                        "Internal error: ExportCopyRelocation sent to non-dynamic object for: {}",
                        resources.symbol_debug(symbol_id)
                    )
                }
            },
            WorkItem::LoadGcUnit(request) => match self {
                FileLayoutState::Object(object_layout_state) => P::load_gc_unit::<A>(
                    object_layout_state,
                    common,
                    resources,
                    queue,
                    request.gc_unit,
                    scope,
                ),
                _ => bail!("Request to load GC unit from non-object: {self}"),
            },
            WorkItem::ExportDynamic(symbol_id) => match self {
                FileLayoutState::Object(object) => {
                    object.export_dynamic::<A>(common, symbol_id, resources, queue, scope)
                }
                _ => {
                    // Non-loaded and dynamic objects don't do anything in response to a request to
                    // export a dynamic symbol.
                    Ok(())
                }
            },
        }
    }

    pub(crate) fn handle_symbol_request<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        symbol_id: SymbolId,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        match self {
            FileLayoutState::Object(state) => {
                SymbolRequestHandler::load_symbol::<A>(
                    state, common, symbol_id, resources, queue, scope,
                )?;
            }
            FileLayoutState::Prelude(state) => {
                SymbolRequestHandler::load_symbol::<A>(
                    state, common, symbol_id, resources, queue, scope,
                )?;
            }
            FileLayoutState::Dynamic(state) => {
                SymbolRequestHandler::load_symbol::<A>(
                    state, common, symbol_id, resources, queue, scope,
                )?;
            }
            FileLayoutState::LinkerScript(_) => {}
            FileLayoutState::StubLibrary(state) => {
                P::load_stub_library_symbol(state, symbol_id)?;
            }
            FileLayoutState::NotLoaded(_) => {}
            FileLayoutState::SyntheticSymbols(state) => {
                SymbolRequestHandler::load_symbol::<A>(
                    state, common, symbol_id, resources, queue, scope,
                )?;
            }
            FileLayoutState::Epilogue(_) => {
                // The epilogue doesn't define symbols. In fact, it isn't even created until after
                // the GC phase graph traversal.
                unreachable!();
            }
        }
        Ok(())
    }

    pub(crate) fn finalise_layout(
        self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut sharded_vec_writer::Shard<Option<Resolution<P>>>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result<FileLayout<'data, P>> {
        let resolutions_out = &mut ResolutionWriter { resolutions_out };

        let file_layout = match self {
            Self::Object(s) => {
                let _span = tracing::debug_span!(
                    "finalise_layout",
                    file = %s.input
                )
                .entered();
                FileLayout::Object(s.finalise_layout(memory_offsets, resolutions_out, resources)?)
            }
            Self::Prelude(s) => FileLayout::Prelude(s.finalise_layout(
                memory_offsets,
                resolutions_out,
                resources,
            )?),
            Self::Epilogue(s) => {
                FileLayout::Epilogue(s.finalise_layout(memory_offsets, resources)?)
            }
            Self::SyntheticSymbols(s) => FileLayout::SyntheticSymbols(s.finalise_layout(
                memory_offsets,
                resolutions_out,
                resources,
            )?),
            Self::Dynamic(s) => s.finalise_layout(memory_offsets, resolutions_out, resources)?,
            Self::StubLibrary(s) => {
                s.finalise_layout(memory_offsets, resolutions_out, resources)?
            }
            Self::LinkerScript(s) => {
                s.finalise_layout(memory_offsets, resolutions_out, resources)?;
                FileLayout::LinkerScript(s)
            }
            Self::NotLoaded(s) => {
                for _ in 0..s.symbol_id_range.len() {
                    resolutions_out.write(None)?;
                }
                FileLayout::NotLoaded
            }
        };

        Ok(file_layout)
    }
}

impl<P: Platform> std::fmt::Display for PreludeLayoutState<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt("<prelude>", f)
    }
}

impl<P: Platform> std::fmt::Display for EpilogueLayoutState<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt("<epilogue>", f)
    }
}

impl<P: Platform> std::fmt::Display for SyntheticSymbolsLayoutState<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt("<synthetic>", f)
    }
}

impl<P: Platform> std::fmt::Display for LinkerScriptLayoutState<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)
    }
}

impl<P: Platform> std::fmt::Display for StubLibraryLayoutState<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)
    }
}

impl<'data, P: Platform> std::fmt::Display for FileLayoutState<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileLayoutState::Object(s) => std::fmt::Display::fmt(s, f),
            FileLayoutState::Dynamic(s) => std::fmt::Display::fmt(s, f),
            FileLayoutState::StubLibrary(s) => std::fmt::Display::fmt(s, f),
            FileLayoutState::LinkerScript(s) => std::fmt::Display::fmt(s, f),
            FileLayoutState::Prelude(_) => std::fmt::Display::fmt("<prelude>", f),
            FileLayoutState::SyntheticSymbols(_) => std::fmt::Display::fmt("<synthetic>", f),
            FileLayoutState::NotLoaded(_) => std::fmt::Display::fmt("<not-loaded>", f),
            FileLayoutState::Epilogue(_) => std::fmt::Display::fmt("<epilogue>", f),
        }
    }
}

impl<'data, P: Platform> std::fmt::Display for FileLayout<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Object(s) => std::fmt::Display::fmt(s, f),
            Self::Dynamic(s) => std::fmt::Display::fmt(s, f),
            Self::LinkerScript(s) => std::fmt::Display::fmt(s, f),
            Self::Prelude(_) => std::fmt::Display::fmt("<prelude>", f),
            Self::Epilogue(_) => std::fmt::Display::fmt("<epilogue>", f),
            Self::SyntheticSymbols(_) => std::fmt::Display::fmt("<synthetic>", f),
            Self::StubLibrary(_) => std::fmt::Display::fmt("<stub-library>", f),
            Self::NotLoaded => std::fmt::Display::fmt("<not loaded>", f),
        }
    }
}

impl<'data, P: Platform> std::fmt::Display for GroupLayout<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.files.len() == 1 {
            self.files[0].fmt(f)
        } else {
            write!(
                f,
                "Group with {} files. Rerun with {}=1",
                self.files.len(),
                crate::args::FILES_PER_GROUP_ENV
            )
        }
    }
}

impl<'data, P: Platform> std::fmt::Display for GroupState<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.files.len() == 1 {
            self.files[0].fmt(f)
        } else {
            write!(
                f,
                "Group with {} files. Rerun with {}=1",
                self.files.len(),
                crate::args::FILES_PER_GROUP_ENV
            )
        }
    }
}

impl<'data, P: Platform> std::fmt::Debug for FileLayout<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self, f)
    }
}

impl<'data, P: Platform> std::fmt::Display for ObjectLayoutState<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)?;
        // TODO: This is mostly for debugging use. Consider only showing this if some environment
        // variable is set, or only in debug builds.
        write!(f, " ({})", self.file_id())
    }
}

impl<'data, P: Platform> std::fmt::Display for DynamicLayoutState<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)?;
        write!(f, " ({})", self.file_id())
    }
}

impl<'data, P: Platform> std::fmt::Display for DynamicLayout<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)?;
        write!(f, " ({})", self.file_id)
    }
}

impl<'data, P: Platform> std::fmt::Display for ObjectLayout<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)?;
        // TODO: This is mostly for debugging use. Consider only showing this if some environment
        // variable is set, or only in debug builds.
        write!(f, " ({})", self.file_id)
    }
}

impl Section {
    pub(crate) fn create<'data, P: Platform>(
        header: &P::SectionHeader,
        object_state: &ObjectLayoutState<'data, P>,
        _part_id: PartId,
    ) -> Result<Section> {
        let size = object_state.object.section_size(header)?;
        let raw_alignment = object_state.object.section_alignment(header)?;
        let alignment = Alignment::new(raw_alignment.max(1))?;
        let section = Section { size, alignment };
        Ok(section)
    }

    // How much space we take up. This is our size rounded up to the next multiple of our
    // alignment, unless we're in a packed section, in which case it's just our size.
    pub(crate) fn capacity<P: Platform>(
        self,
        part_id: PartId,
        output_sections: &OutputSections<P>,
    ) -> u64 {
        if part_id.should_pack::<P>() {
            self.size
        } else {
            part_id.alignment(output_sections).align_up(self.size)
        }
    }

    pub(crate) fn place(self, offset: u64) -> (u64, u64) {
        let address = self.alignment.align_up(offset);
        (address, address + self.size)
    }
}

impl<'data, P: Platform> PreludeLayoutState<'data, P> {
    pub(crate) fn new(input_state: resolution::ResolvedPrelude<'data, P>, args: &P::Args) -> Self {
        Self {
            file_id: PRELUDE_FILE_ID,
            symbol_id_range: SymbolIdRange::prelude(input_state.symbol_definitions.len()),
            internal_symbols: InternalSymbols {
                symbol_definitions: input_state.symbol_definitions,
                start_symbol_id: SymbolId::zero(),
            },
            entry_symbol_id: None,
            identity: format!("Linker: {}\0", args.common().linker_identity()),
            header_info: None,
            dynamic_linker: None,
            format_specific: Default::default(),
        }
    }

    pub(crate) fn activate<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, '_, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        if resources.symbol_db.args.should_write_linker_identity()
            && let Some(comment_section_id) = P::COMMENT_SECTION_ID
        {
            // Allocate space to store the identity of the linker in the .comment section.
            common.allocate(
                comment_section_id.part_id_with_alignment::<P>(alignment::MIN),
                self.identity.len() as u64,
            );
        }

        self.load_entry_point::<A>(resources, queue, scope);

        P::allocate_prelude(common, resources.symbol_db);

        if resources.symbol_db.output_kind.is_dynamic_executable() {
            self.dynamic_linker = resources
                .symbol_db
                .args
                .dynamic_linker()
                .map(|p| CString::new(p.as_os_str().as_encoded_bytes()))
                .transpose()?;
        }
        if let Some(dynamic_linker) = self.dynamic_linker.as_ref() {
            let interp_section_id = P::INTERP_SECTION_ID
                .expect("platform specified a dynamic linker without an interpreter section");
            common.allocate(
                interp_section_id.base_part_id::<P>(),
                dynamic_linker.as_bytes_with_nul().len() as u64,
            );
        }

        self.mark_defsyms_as_used::<A>(resources, queue, scope);

        Ok(())
    }

    /// Mark defsyms from the command-line as being directly referenced so that we emit the symbols
    /// even if nothing in the code references them.
    pub(crate) fn mark_defsyms_as_used<'scope, A: Arch<Platform = P>>(
        &self,
        resources: &'scope GraphResources<'data, '_, A::Platform>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) {
        for (index, def_info) in self.internal_symbols.symbol_definitions.iter().enumerate() {
            let symbol_id = self.symbol_id_range.offset_to_id(index);
            if !resources.symbol_db.is_canonical(symbol_id) {
                continue;
            }

            match &def_info.placement {
                SymbolPlacement::Redirect(redirect) => {
                    load_redirect_referenced_symbols::<A>(
                        resources, queue, scope, symbol_id, redirect,
                    );
                }
                _ => {}
            }
        }
    }

    pub(crate) fn load_entry_point<'scope, A: Arch<Platform = P>>(
        &mut self,
        resources: &'scope GraphResources<'data, '_, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) {
        let Some(entry_name) = resources.symbol_db.entry_symbol_name() else {
            return;
        };
        let Some(symbol_id) = resources
            .symbol_db
            .get_unversioned(&UnversionedSymbolName::prehashed(entry_name))
        else {
            // We'll emit a warning when writing the file if it's an executable.
            return;
        };

        let symbol_id = resources.symbol_db.definition(symbol_id);

        self.entry_symbol_id = Some(symbol_id);
        let file_id = resources.symbol_db.file_id_for_symbol(symbol_id);
        let old_flags = resources
            .per_symbol_flags
            .get_atomic(symbol_id)
            .fetch_or(ValueFlags::DIRECT);
        if !old_flags.has_resolution() {
            queue.send_work::<A>(
                resources,
                file_id,
                WorkItem::LoadGlobalSymbol(symbol_id),
                scope,
            );
        }
    }

    pub(crate) fn finalise_sizes(
        common: &mut CommonGroupState<'data, P>,
        merged_strings: &OutputSectionMap<MergedStringsSection<'data>>,
    ) {
        merged_strings.for_each(|section_id, merged| {
            if merged.len() > 0 {
                common.allocate(
                    section_id.part_id_with_alignment::<P>(alignment::MIN),
                    merged.len(),
                );
            }
        });
    }

    /// This function is where we determine sizes that depend on other sizes. For example, the size
    /// of the section headers table, which depends on which sections we're writing, which depends
    /// on which sections are non-empty. We also decide which internal symtab entries we'll write
    /// here, since that also depends on which sections we're writing.
    pub(crate) fn apply_late_size_adjustments(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        total_sizes: &mut OutputSectionPartMap<u64>,
        must_keep_sections: OutputSectionMap<bool>,
        output_sections: &mut OutputSections<P>,
        output_order: &OutputOrder<'data>,
        program_segments: &ProgramSegments<P::ProgramSegmentDef>,
        per_symbol_flags: &mut PerSymbolFlags,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        // Total section  sizes have already been computed. So any allocations we do need to update
        // both `total_sizes` and the size records in `common`. We track the extra sizes in
        // `extra_sizes` which we can then later add to both.
        let mut extra_sizes = common.mem_sizes.new_empty_like();

        self.determine_header_sizes(
            total_sizes,
            &mut extra_sizes,
            must_keep_sections,
            output_sections,
            program_segments,
            output_order,
            resources,
            per_symbol_flags,
        );

        P::apply_late_size_adjustments_prelude(
            total_sizes,
            &mut extra_sizes,
            resources.format_specific,
            resources.symbol_db.args,
        )?;

        self.allocate_symbol_table_sizes(
            output_sections,
            per_symbol_flags,
            resources.symbol_db,
            &mut extra_sizes,
        )?;

        let entry_size = size_of::<P::SymtabEntry>() as u64;

        if resources.symbol_db.args.should_copy_input_relocs() {
            let mut num_section_syms = 0;
            for (id, _) in output_sections.ids_with_info() {
                if output_sections.will_emit_section_symbol_for_partial_objects(id) {
                    num_section_syms += 1;
                }
            }
            extra_sizes.increment(
                P::SYMTAB_LOCAL_SECTION_ID
                    .expect("copying input relocs requires a local symbol table")
                    .base_part_id::<P>(),
                num_section_syms * entry_size,
            );
        }

        // We need to allocate both our own size record and the group totals, since they've already
        // been computed.
        common.mem_sizes.merge(&extra_sizes);
        total_sizes.merge(&extra_sizes);

        Ok(())
    }

    /// Allocates space for our internal symbols. For unreferenced symbols, we also update the
    /// symbol so that it is treated as referenced, but only for symbols in sections that we're
    /// going to emit.
    pub(crate) fn allocate_symbol_table_sizes(
        &self,
        output_sections: &OutputSections<P>,
        per_symbol_flags: &mut PerSymbolFlags,
        symbol_db: &SymbolDb<'data, P>,
        extra_sizes: &mut OutputSectionPartMap<u64>,
    ) -> Result<(), Error> {
        if symbol_db.args.should_strip_all() {
            return Ok(());
        }

        self.internal_symbols.allocate_symbol_table_sizes(
            extra_sizes,
            symbol_db,
            |symbol_id, def_info| {
                if def_info.name.is_empty() {
                    return false;
                }

                let flags = per_symbol_flags.flags_for_symbol(symbol_id);

                // If the symbol is referenced, then we keep it.
                if flags.has_resolution() {
                    return true;
                }

                // We always emit symbols that the user requested be undefined.
                let mut should_emit = matches!(def_info.placement, SymbolPlacement::ForceUndefined);

                // Keep the symbol if we're going to write the section, even though the symbol isn't
                // referenced. It can be useful to have symbols like _GLOBAL_OFFSET_TABLE_ when
                // using a debugger. In partial-link mode, skip symbols that point to internal
                // metadata sections (file header, program headers, section headers, symtab, strtab)
                // since those are not meaningful in a relocatable object.
                should_emit |= def_info.section_id().is_some_and(|sec_id| {
                    // GNU ld defines `__ehdr_start` only when referenced (PROVIDE_HIDDEN).
                    // FILE_HEADER is always kept for ELF header space, which would otherwise
                    // put an unreferenced `__ehdr_start` in `.symtab`.
                    if sec_id == crate::output_section_id::FILE_HEADER {
                        return false;
                    }
                    if symbol_db.args.should_output_partial_object() {
                        output_sections.will_emit_section_symbol_for_partial_objects(sec_id)
                    } else {
                        output_sections.will_emit_section(sec_id)
                    }
                });

                if should_emit {
                    // Mark the symbol as referenced so that we later generate a resolution for
                    // it and subsequently write it to the symbol table.
                    per_symbol_flags.set_flag(symbol_id, ValueFlags::DIRECT);
                }

                should_emit
            },
        )
    }

    pub(crate) fn determine_header_sizes(
        &mut self,
        total_sizes: &OutputSectionPartMap<u64>,
        extra_sizes: &mut OutputSectionPartMap<u64>,
        must_keep_sections: OutputSectionMap<bool>,
        output_sections: &mut OutputSections<P>,
        program_segments: &ProgramSegments<P::ProgramSegmentDef>,
        output_order: &OutputOrder<'data>,
        resources: &FinaliseSizesResources<'data, '_, P>,
        symbol_flags: &PerSymbolFlags,
    ) {
        use output_section_id::OrderEvent;

        // Empty object sections with symbols must still be emitted
        // (empty-section-alignment). Script-only markers with no inputs are
        // omitted later, matching GNU ld (kernel `.init.begin`, `.builtin_fw`).
        let mut loaded_empty_input = vec![false; output_sections.num_sections()];
        for i in 0..output_sections.num_sections() {
            let section_id = OutputSectionId::from_usize(i);
            if *must_keep_sections.get(section_id) {
                let primary = output_sections.primary_output_section(section_id);
                loaded_empty_input[primary.as_usize()] = true;
            }
        }

        // Determine which sections to keep. To start with, we keep all sections that we've
        // previously marked as needing to be kept. These may include sections that are empty, but
        // into which we've loaded an empty input section.
        let mut keep_sections = must_keep_sections;

        // Next, keep any sections for which we've recorded a non-zero size.
        total_sizes.map(|part_id, size| {
            if *size > 0 {
                *keep_sections.get_mut(part_id.output_section_id::<P>()) = true;
            }
        });

        // Keep any sections that we've said we want to keep regardless.
        P::apply_force_keep_sections(&mut keep_sections, resources.symbol_db.args);

        // Keep any sections that have a start/stop symbol which is referenced.
        symbol_flags
            .raw_range(self.symbol_id_range())
            .iter()
            .zip(self.internal_symbols.symbol_definitions.iter())
            .for_each(|(raw_flags, definition)| {
                if raw_flags.get().has_resolution()
                    && let Some(section_id) = definition.section_id()
                {
                    *keep_sections.get_mut(section_id) = true;
                }
            });

        for i in 0..output_sections.num_sections() {
            let section_id = OutputSectionId::from_usize(i);

            // If any secondary sections were marked to be kept, then unmark them and mark the
            // primary instead.
            if let Some(primary_id) = output_sections.merge_target(section_id) {
                let keep_secondary = replace(keep_sections.get_mut(section_id), false);
                *keep_sections.get_mut(primary_id) |= keep_secondary;
            }

            // Remove any built-in sections without a type except for section 0 (the file header).
            // This should just be the .phdr and .shdr sections which contain the program headers
            // and section headers. We need these sections in order to allocate space for those
            // structures, but other linkers don't emit section headers for them, so neither should
            // we. Custom sections (e.g. from linker scripts) that still have NULL type get the
            // default section type assigned instead, since an empty but explicitly defined section
            // should still be emitted if something references it.
            let section_info = output_sections.section_infos.get(section_id);
            if section_info.section_attributes.is_null()
                && section_id != crate::output_section_id::FILE_HEADER
            {
                if section_id.is_custom::<P>() {
                    let has_output_data = output_sections.script_output_data.iter().any(|data| {
                        output_sections.primary_output_section(data.section_id) == section_id
                    });
                    let info = output_sections.section_infos.get_mut(section_id);
                    info.section_attributes.set_to_default_type();
                    if !info.section_attributes.avoids_alloc() {
                        let explicit_zero = info
                            .location_info
                            .as_ref()
                            .is_some_and(|loc| matches!(loc.location, Some(Expression::Number(0))));
                        let loadable = !info.phdrs.is_empty();
                        let writable = script_phdrs_writable(&info.phdrs, resources.symbol_db);
                        if !explicit_zero && loadable {
                            info.section_attributes.set_alloc();
                            if writable {
                                info.section_attributes.set_writable();
                            }
                            if !has_output_data {
                                info.section_attributes.set_no_bits();
                            }
                        }
                    }
                } else {
                    *keep_sections.get_mut(section_id) = false;
                }
            }
        }

        // GNU ld omits empty output sections that never received an input and
        // have no `. +=` / BYTE data. `.orc_lookup { . += N; }` stays because
        // it has a relative location counter even before that size is known.
        let mut content_size = vec![0u64; output_sections.num_sections()];
        for i in 0..output_sections.num_sections() {
            let section_id = OutputSectionId::from_usize(i);
            let primary = output_sections.primary_output_section(section_id);
            for (_, &part_size) in total_sizes.in_range(section_id.part_id_range::<P>()) {
                content_size[primary.as_usize()] += part_size;
            }
        }
        let mut has_relative_lc = vec![false; output_sections.num_sections()];
        for event in output_order {
            if let OrderEvent::SetLocationRelative(_, section_id, ..) = event {
                has_relative_lc[section_id.as_usize()] = true;
            }
        }
        for data in &output_sections.script_output_data {
            let primary = output_sections.primary_output_section(data.section_id);
            has_relative_lc[primary.as_usize()] = true;
        }
        for i in 0..output_sections.num_sections() {
            let section_id = OutputSectionId::from_usize(i);
            if !section_id.is_custom::<P>() || output_sections.merge_target(section_id).is_some() {
                continue;
            }
            if has_relative_lc[i] {
                *keep_sections.get_mut(section_id) = true;
            } else if content_size[i] == 0 && !loaded_empty_input[i] {
                *keep_sections.get_mut(section_id) = false;
            }
        }

        let num_keep = keep_sections.values_iter().filter(|p| **p).count();
        if P::requires_symtab_shndx(num_keep) {
            *keep_sections.get_mut(
                P::SYMTAB_SHNDX_LOCAL_SECTION_ID
                    .expect("platform requires a symbol-table section-index table"),
            ) = true;
        }

        // Compute output indexes of each section.
        let mut next_output_index = 0;
        let mut output_section_indexes = vec![None; output_sections.num_sections()];
        for event in output_order {
            if let OrderEvent::Section(id) = event
                && *keep_sections.get(id)
            {
                debug_assert!(
                    output_sections.merge_target(id).is_none(),
                    "Tried to allocate section header for secondary section {}",
                    output_sections.section_debug(id)
                );
                output_section_indexes[id.as_usize()] = Some(next_output_index);
                next_output_index += 1;
            }
        }
        output_sections.output_section_indexes = output_section_indexes;
        // Only sections that appear in the output order receive a section header. Custom
        // PHDRS order can omit some kept builtins; size the table from the indexes we assigned.
        let num_sections = next_output_index;

        // Determine which program segments contain sections that we're keeping.
        let mut keep_segments = if program_segments.has_custom_phdrs() {
            vec![true; program_segments.len()]
        } else {
            let mut keep_segments = program_segments
                .iter()
                .map(|details| details.always_keep())
                .collect_vec();
            let mut active_segments = Vec::with_capacity(4);
            for event in output_order {
                match event {
                    OrderEvent::SegmentStart(segment_id) => active_segments.push(segment_id),
                    OrderEvent::SegmentEnd(segment_id) => {
                        active_segments.retain(|a| *a != segment_id);
                    }
                    OrderEvent::Section(section_id) => {
                        if *keep_sections.get(section_id) {
                            for segment_id in &active_segments {
                                keep_segments[segment_id.as_usize()] = true;
                            }
                            active_segments.clear();
                        }
                    }
                    OrderEvent::SetLocation(..)
                    | OrderEvent::SetLocationRelative(..)
                    | OrderEvent::SetSectionAddress(_) => {}
                }
            }

            if !resources.symbol_db.args.should_output_partial_object() {
                // Always keep the program headers segment even though we don't emit any sections in
                // it.
                keep_segments[0] = true;
            }
            keep_segments
        };
        P::update_segment_keep_list(
            program_segments,
            &mut keep_segments,
            resources.symbol_db.args,
        );

        let active_segment_ids = if resources.symbol_db.args.should_output_partial_object() {
            vec![]
        } else {
            (0..program_segments.len())
                .map(ProgramSegmentId::new)
                .filter(|id| keep_segments[id.as_usize()] || program_segments.is_stack_segment(*id))
                .collect()
        };

        let header_info = HeaderInfo {
            num_output_sections_with_content: num_sections
                .try_into()
                .expect("output section count must fit in a u32"),

            active_segment_ids,
        };

        // Allocate space for headers based on segment and section counts.
        P::allocate_header_sizes(
            self,
            extra_sizes,
            &header_info,
            program_segments,
            output_sections,
            resources,
            resources.symbol_db.args,
        );

        self.header_info = Some(header_info);
    }

    pub(crate) fn finalise_layout(
        self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut ResolutionWriter<P>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result<PreludeLayout<'data, P>> {
        let header_layout = resources
            .section_layouts
            .get(crate::output_section_id::FILE_HEADER);
        assert_eq!(header_layout.file_offset, 0);

        let format_specific = P::finalise_prelude_layout(&self, memory_offsets, resources)?;

        self.internal_symbols
            .finalise_layout(memory_offsets, resolutions_out, resources)?;

        if resources.symbol_db.args.should_write_linker_identity()
            && let Some(comment_section_id) = P::COMMENT_SECTION_ID
        {
            memory_offsets.increment(
                comment_section_id.part_id_with_alignment::<P>(alignment::MIN),
                self.identity.len() as u64,
            );
        }

        resources.merged_strings.for_each(|section_id, merged| {
            if merged.len() > 0 {
                memory_offsets.increment(
                    section_id.part_id_with_alignment::<P>(alignment::MIN),
                    merged.len(),
                );
            }
        });

        Ok(PreludeLayout {
            internal_symbols: self.internal_symbols,
            entry_symbol_id: self.entry_symbol_id,
            identity: self.identity,
            dynamic_linker: self.dynamic_linker,
            header_info: self
                .header_info
                .expect("we should have computed header info by now"),
            format_specific,
        })
    }
}

impl<'data, P: Platform> InternalSymbols<'data, P> {
    pub(crate) fn activate_symbols<'scope, A: Arch<Platform = P>>(
        &self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, '_, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        for (offset, def_info) in self.symbol_definitions.iter().enumerate() {
            let symbol_id = self.start_symbol_id.add_usize(offset);
            if !resources.symbol_db.is_canonical(symbol_id) {
                continue;
            }

            // PROVIDE_HIDDEN symbols should not be exported to dynsym.
            if def_info.symbol.is_hidden() {
                if def_info.is_provide
                    && let SymbolPlacement::Redirect(redirect) = &def_info.placement
                {
                    load_redirect_expression_targets::<A>(resources, queue, scope, redirect);
                }
                continue;
            }

            match &def_info.placement {
                SymbolPlacement::Redirect(redirect) => {
                    if def_info.is_provide {
                        load_redirect_expression_targets::<A>(resources, queue, scope, redirect);
                    } else {
                        load_redirect_referenced_symbols::<A>(
                            resources, queue, scope, symbol_id, redirect,
                        );
                    }
                }
                _ => {}
            }

            if def_info.name.is_empty() {
                continue;
            }

            if def_info.is_provide && provide_has_missing_rhs(def_info, resources.symbol_db) {
                continue;
            }

            resources
                .per_symbol_flags
                .get_atomic(symbol_id)
                .fetch_or(ValueFlags::EXPORT_DYNAMIC);

            if resources.symbol_db.output_kind.needs_dynsym() {
                export_dynamic(common, symbol_id, resources.symbol_db)?;
            }
        }

        Ok(())
    }

    pub(crate) fn allocate_symbol_table_sizes(
        &self,
        sizes: &mut OutputSectionPartMap<u64>,
        symbol_db: &SymbolDb<'data, P>,
        mut should_keep_symbol: impl FnMut(SymbolId, &InternalSymDefInfo<P>) -> bool,
    ) -> Result {
        // Allocate space in the symbol table for the symbols that we define.
        for (index, def_info) in self.symbol_definitions.iter().enumerate() {
            if def_info.name.is_empty() {
                continue;
            }
            let symbol_id = self.start_symbol_id.add_usize(index);
            if !symbol_db.is_canonical(symbol_id) || symbol_id.is_undefined() {
                continue;
            }

            if !should_keep_symbol(symbol_id, def_info) {
                continue;
            }

            P::allocate_internal_symbol(symbol_id, def_info, sizes, symbol_db)?;
        }
        Ok(())
    }

    pub(crate) fn finalise_layout(
        &self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut ResolutionWriter<P>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result {
        // Define symbols that are optionally put at the start/end of some sections.
        for (local_index, def_info) in self.symbol_definitions.iter().enumerate() {
            let symbol_id = self.start_symbol_id.add_usize(local_index);

            let resolution =
                create_internal_symbol_resolution(memory_offsets, resources, def_info, symbol_id);

            resolutions_out.write(resolution)?;
        }
        Ok(())
    }

    pub(crate) fn symbol_id_range(&self) -> SymbolIdRange {
        SymbolIdRange::input(self.start_symbol_id, self.symbol_definitions.len())
    }
}

impl<'data, P: Platform> SyntheticSymbolsLayoutState<'data, P> {
    pub(crate) fn new(
        input_state: resolution::ResolvedSyntheticSymbols<'data, P>,
    ) -> SyntheticSymbolsLayoutState<'data, P> {
        SyntheticSymbolsLayoutState {
            file_id: input_state.file_id,
            symbol_id_range: SymbolIdRange::input(
                input_state.start_symbol_id,
                input_state.symbol_definitions.len(),
            ),
            internal_symbols: InternalSymbols {
                symbol_definitions: input_state.symbol_definitions,
                start_symbol_id: input_state.start_symbol_id,
            },
        }
    }

    pub(crate) fn finalise_sizes(
        &self,
        common: &mut CommonGroupState<'data, P>,
        per_symbol_flags: &AtomicPerSymbolFlags,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        let symbol_db = resources.symbol_db;

        if !symbol_db.args.should_strip_all() {
            self.internal_symbols.allocate_symbol_table_sizes(
                &mut common.mem_sizes,
                symbol_db,
                |symbol_id, _| {
                    // For user-defined start/stop symbols, we only emit them if they're referenced.
                    per_symbol_flags
                        .flags_for_symbol(symbol_id)
                        .has_resolution()
                },
            )?;
        }

        Ok(())
    }

    pub(crate) fn finalise_layout(
        self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut ResolutionWriter<P>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result<SyntheticSymbolsLayout<'data, P>> {
        self.internal_symbols
            .finalise_layout(memory_offsets, resolutions_out, resources)?;

        Ok(SyntheticSymbolsLayout {
            internal_symbols: self.internal_symbols,
        })
    }
}

impl<'data, P: Platform> EpilogueLayoutState<P> {
    pub(crate) fn new(
        args: &P::Args,
        output_kind: OutputKind,
        dynamic_symbol_definitions: &mut [DynamicSymbolDefinition<'data, P>],
        group_states: &[GroupState<'data, P>],
    ) -> Self {
        EpilogueLayoutState {
            format_specific: P::new_epilogue_layout(
                args,
                output_kind,
                dynamic_symbol_definitions,
                group_states,
            ),
        }
    }

    pub(crate) fn apply_late_size_adjustments(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        total_sizes: &mut OutputSectionPartMap<u64>,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        let mut extra_sizes = common.mem_sizes.new_empty_like();
        for sec in resources.script_sorted_sections {
            extra_sizes.increment(sec.part_id, sec.size);
        }
        P::apply_late_size_adjustments_epilogue(
            &mut self.format_specific,
            total_sizes,
            &mut extra_sizes,
            resources.dynamic_symbol_definitions,
            resources.format_specific,
            resources.symbol_db.args,
        )?;

        // See comments in Prelude::apply_late_size_adjustments.
        total_sizes.merge(&extra_sizes);
        common.mem_sizes.merge(&extra_sizes);

        Ok(())
    }

    pub(crate) fn finalise_sizes(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) {
        let symbol_db = resources.symbol_db;

        P::finalise_sizes_epilogue(
            &mut self.format_specific,
            &mut common.mem_sizes,
            resources.dynamic_symbol_definitions,
            resources.format_specific,
            symbol_db,
        );
    }

    pub(crate) fn finalise_layout(
        mut self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result<EpilogueLayout<P>> {
        let dynsym_start_index = P::DYNSYM_SECTION_ID
            .and_then(|section_id| {
                P::single_part_id(section_id).map(|part_id| (section_id, part_id))
            })
            .map(|(section_id, part_id)| {
                ((memory_offsets.get(part_id)
                    - resources.section_layouts.get(section_id).mem_offset)
                    / size_of::<P::SymtabEntry>() as u64)
                    .try_into()
                    .context("Too many dynamic symbols")
            })
            .transpose()?
            .unwrap_or(0);

        P::finalise_layout_epilogue(
            &mut self.format_specific,
            memory_offsets,
            resources.symbol_db,
            resources.format_specific,
            dynsym_start_index,
            resources.dynamic_symbol_definitions,
        )?;
        relocate_gnu_build_id_layout_offset(memory_offsets, resources.output_sections);
        for sec in resources.script_sorted_sections {
            let offset = memory_offsets.get_mut(sec.part_id);
            *offset = sec.alignment.align_up(*offset);
            *offset += sec.size;
        }
        Ok(EpilogueLayout {
            format_specific: self.format_specific,
            dynsym_start_index,
        })
    }
}

#[derive(Debug)]
pub(crate) struct HeaderInfo {
    pub(crate) num_output_sections_with_content: u32,
    pub(crate) active_segment_ids: Vec<ProgramSegmentId>,
}

impl<'data, P: Platform> ObjectLayoutState<'data, P> {
    #[inline(always)]
    pub(crate) fn activate<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        P::activate_object_gc::<A>(self, common, resources, queue, scope)?;

        if let Some(mode) = export_symbols_mode(resources.symbol_db, &self.input) {
            self.load_non_hidden_symbols::<A>(common, resources, queue, mode, scope)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportSymbolsMode {
    Selected,
    All,
}

impl<'data, P: Platform<GcUnit = SectionGcUnit>> ObjectLayoutState<'data, P> {
    pub(crate) fn activate_section_gc<'scope, A>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result
    where
        A: Arch<Platform = P>,
    {
        let mut frame_section_indices = SmallVec::<[SectionIndex; 2]>::new();
        let mut note_gnu_property_section = None;
        let mut riscv_attributes_section = None;

        let no_gc = !resources.symbol_db.args.should_gc_sections();

        for (i, section) in self.sections.iter().enumerate() {
            match section {
                SectionSlot::MustLoad(..)
                | SectionSlot::UnloadedDebugInfo
                | SectionSlot::MergeStrings(_) => {
                    queue.send_gc_unit_request::<A>(
                        self.file_id,
                        SectionGcUnit::new(object::SectionIndex(i)),
                        resources,
                        scope,
                    );
                }
                SectionSlot::Unloaded(sec) => {
                    if no_gc {
                        queue.send_gc_unit_request::<A>(
                            self.file_id,
                            SectionGcUnit::new(object::SectionIndex(i)),
                            resources,
                            scope,
                        );
                    } else if sec.start_stop_eligible {
                        let part_id = self.section_part_id(
                            object::SectionIndex(i),
                            &resources.symbol_db.section_part_ids,
                        );
                        resources
                            .start_stop_sections
                            .get(part_id.output_section_id::<P>())
                            .push(GcLoadRequest::new(
                                self.file_id,
                                SectionGcUnit::new(object::SectionIndex(i)),
                            ));
                    }
                }
                SectionSlot::FrameData(index) => {
                    frame_section_indices.push(*index);
                }
                SectionSlot::NoteGnuProperty(index) => {
                    note_gnu_property_section = Some(*index);
                }
                SectionSlot::RiscvVAttributes(index) => {
                    riscv_attributes_section = Some(*index);
                }
                _ => (),
            }
        }

        for frame_data_section_index in frame_section_indices {
            <A::Platform as Platform>::load_exception_frame_data::<A>(
                self,
                common,
                frame_data_section_index,
                resources,
                queue,
                scope,
            )?;
        }

        if let Some(section_index) = note_gnu_property_section {
            self.object
                .process_gnu_note_section(&mut self.format_specific, section_index)?;
        }

        if let Some(riscv_attributes_index) = riscv_attributes_section {
            A::process_riscv_attributes(
                self.object,
                &mut self.format_specific,
                riscv_attributes_index,
            )
            .context("Cannot parse .riscv.attributes section")?;
        }

        Ok(())
    }
}

impl<'data, P: Platform> ObjectLayoutState<'data, P> {
    pub(crate) fn handle_section_load_request<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        section_index: SectionIndex,
        scope: &Scope<'scope>,
    ) -> Result<(), Error> {
        match &self.sections[section_index.0] {
            SectionSlot::Unloaded(unloaded) | SectionSlot::MustLoad(unloaded) => {
                self.load_section::<A>(common, queue, *unloaded, section_index, resources, scope)?;
            }
            SectionSlot::UnloadedDebugInfo => {
                // On RISC-V, the debug info sections contain relocations to local symbols (e.g.
                // labels).
                self.load_debug_section::<A>(common, section_index, resources)?;
            }
            SectionSlot::Discard => {
                bail!(
                    "{self}: Don't know what segment to put `{}` in, but it's referenced",
                    self.object.section_display_name(section_index),
                );
            }
            SectionSlot::Loaded(_)
            | SectionSlot::Sorted(_)
            | SectionSlot::FrameData(..)
            | SectionSlot::LoadedDebugInfo(..)
            | SectionSlot::NoteGnuProperty(..)
            | SectionSlot::RiscvVAttributes(..) => {}
            SectionSlot::MergeStrings(_) => {
                // We currently always load everything in merge-string sections. i.e. we don't GC
                // unreferenced data. So the only thing we need to do here is propagate section
                // flags.
                let header = self.object.section(section_index)?;
                let part_id =
                    self.section_part_id(section_index, &resources.symbol_db.section_part_ids);
                common.store_section_attributes(part_id, header);
            }
        }

        Ok(())
    }

    pub(crate) fn load_section<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        queue: &mut LocalWorkQueue<P>,
        unloaded: UnloadedSection,
        section_index: SectionIndex,
        resources: &'scope GraphResources<'data, 'scope, P>,
        scope: &Scope<'scope>,
    ) -> Result {
        let part_id = self.section_part_id(section_index, &resources.symbol_db.section_part_ids);
        let header = self.object.section(section_index)?;

        // Warn about RWX sections like GNU ld does, as they pose a security risk.
        if header.is_alloc() && header.is_writable() && header.is_executable() {
            resources.symbol_db.warning(format!(
                "{}: section `{}` has RWX (read+write+execute) permissions",
                self.input,
                self.object.section_display_name(section_index),
            ));
        }

        let section = Section::create(header, self, part_id)?;

        <A::Platform as Platform>::load_object_section_relocations::<A>(
            self,
            common,
            queue,
            resources,
            section,
            section_index,
            scope,
        )?;

        tracing::debug!(loaded_section = %self.object.section_display_name(section_index), file = %self.input);

        self.sections[section_index.0] = if unloaded.needs_sorting {
            self.script_sorted_sections.push(ScriptSortedSectionDetail {
                index: section_index,
                sort_by_init_priority: unloaded.sort_by_init_priority,
            });
            SectionSlot::Sorted(SortedSection {
                // Filled in later.
                address: 0,
                section,
            })
        } else {
            common.allocate(
                part_id,
                section.capacity(part_id, resources.output_sections),
            );
            SectionSlot::Loaded(section)
        };

        common.store_section_attributes(part_id, header);

        if let Some(config) = A::thunk_config()
            && resources.thunk_layout_builder.is_some()
            && part_id == config.primary_function_part_id
        {
            self.post_gc_primary_bytes += section.size;
        }

        let section_id = part_id.output_section_id::<P>();

        if section.size > 0 {
            P::non_empty_section_loaded::<A>(self, common, queue, unloaded, resources, scope)?;
        } else if P::is_zero_sized_section_content(section_id) {
            resources.keep_section(section_id);
        }

        P::load_associated_reloc_sections::<A>(
            self,
            common,
            queue,
            resources,
            section_index,
            scope,
        )?;

        Ok(())
    }

    pub(crate) fn load_debug_section<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        section_index: SectionIndex,
        resources: &'scope GraphResources<'data, '_, P>,
    ) -> Result {
        let part_id = self.section_part_id(section_index, &resources.symbol_db.section_part_ids);
        let header = self.object.section(section_index)?;
        let section = Section::create(header, self, part_id)?;

        // Note: We intentionally do NOT process debug relocations here. On some architectures (like
        // RISC-V and LoongArch64), debug sections reference local symbols (e.g. .LFB0, .LFE0) in
        // code sections. Processing those relocations during GC would send symbol requests that
        // load those code sections, defeating garbage collection. Instead, debug relocations are
        // resolved at write time in `apply_debug_relocation`, which uses tombstone values for
        // symbols in GC'd sections and computes addresses from section resolutions for symbols in
        // live sections.

        tracing::debug!(loaded_debug_section = %self.object.section_display_name(section_index),);
        common.allocate(
            part_id,
            section.capacity(part_id, resources.output_sections),
        );
        common.store_section_attributes(part_id, header);
        self.sections[section_index.0] = SectionSlot::LoadedDebugInfo(section);

        Ok(())
    }

    pub(crate) fn finalise_sizes(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        per_symbol_flags: &AtomicPerSymbolFlags,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        if !resources.symbol_db.args.should_strip_all() {
            self.allocate_symtab_space(common, resources.symbol_db, per_symbol_flags)?;
        }
        let output_kind = resources.symbol_db.output_kind;
        for slot in &mut self.sections {
            if let SectionSlot::Loaded(_) = slot {
                P::allocate_resolution(
                    ValueFlags::empty(),
                    &mut common.mem_sizes,
                    output_kind,
                    resources.symbol_db.args,
                );
            }
        }

        P::finalise_object_sizes(self, common);

        Ok(())
    }

    pub(crate) fn allocate_symtab_space(
        &self,
        common: &mut CommonGroupState<'data, P>,
        symbol_db: &SymbolDb<'data, P>,
        per_symbol_flags: &AtomicPerSymbolFlags,
    ) -> Result {
        let _file_span = symbol_db.args.common().trace_span_for_file(self.file_id());
        P::allocate_object_symtab_space(self, common, symbol_db, per_symbol_flags)
    }

    pub(crate) fn finalise_layout(
        mut self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut ResolutionWriter<P>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result<ObjectLayout<'data, P>> {
        let _file_span = resources
            .symbol_db
            .args
            .common()
            .trace_span_for_file(self.file_id());
        let symbol_id_range = self.symbol_id_range();

        let sframe_section_id = P::SFRAME_SECTION_ID;
        let sframe_start_address = sframe_section_id
            .map(|section_id| resources.section_layouts.get(section_id).mem_offset);
        let mut sframe_ranges = Vec::new();

        let mut section_resolutions = Vec::with_capacity(self.sections.len());
        let section_id_range = self.section_id_range;
        let object_part_ids = &resources.symbol_db.section_part_ids[section_id_range.as_usize()];

        for (slot, &part_id) in self.sections.iter_mut().zip(object_part_ids) {
            let resolution = match slot {
                SectionSlot::Loaded(sec) => {
                    let mut offset = memory_offsets.get(part_id);
                    let address = advance_section_offset(
                        &mut offset,
                        *sec,
                        part_id,
                        resources.output_sections,
                    );
                    *memory_offsets.get_mut(part_id) = offset;

                    // TODO: We probably need to be able to handle sections that are ifuncs and
                    // sections that need a TLS GOT struct.

                    // Collect SFrame section ranges while we're already iterating
                    if Some(part_id.output_section_id::<P>()) == sframe_section_id {
                        let offset = (address - sframe_start_address.unwrap()) as usize;
                        let len = sec.size as usize;
                        sframe_ranges.push(offset..offset + len);
                    }

                    SectionResolution { address }
                }

                SectionSlot::Sorted(sec) => SectionResolution {
                    address: sec.address,
                },

                &mut SectionSlot::LoadedDebugInfo(sec) => {
                    let mut offset = memory_offsets.get(part_id);
                    let address = advance_section_offset(
                        &mut offset,
                        sec,
                        part_id,
                        resources.output_sections,
                    );
                    *memory_offsets.get_mut(part_id) = offset;
                    SectionResolution { address }
                }
                SectionSlot::FrameData(..) => {
                    let address = P::frame_data_base_address(memory_offsets);
                    SectionResolution { address }
                }
                _ => SectionResolution::none(),
            };
            section_resolutions.push(resolution);
        }

        for ((local_symbol_index, local_symbol), &flags) in self
            .object
            .enumerate_symbols()
            .zip(resources.per_symbol_flags.raw_range(symbol_id_range))
        {
            self.finalise_symbol(
                resources,
                flags.get(),
                local_symbol,
                local_symbol_index,
                &section_resolutions,
                memory_offsets,
                resolutions_out,
            )?;
        }

        P::finalise_object_layout(&self, memory_offsets);

        // If this object owns a ThunkBlock, assign addresses for the block's thunks and write
        // them directly into the shared output map.
        if self.owns_thunk_block
            && let Some(config) = P::file_thunk_config(self.object)
            && let Some(block) = resources.thunk_blocks.get(self.thunk_block_id.as_usize())
            && !block.symbols.is_empty()
        {
            let mut addresses = resources.thunk_block_addresses[self.thunk_block_id.as_usize()]
                .lock()
                .unwrap();

            let addr = memory_offsets.get_mut(config.primary_function_part_id);
            for &symbol_id in &block.symbols {
                addresses.insert(symbol_id, *addr);
                *addr += config.thunk_size;
            }
        }

        Ok(ObjectLayout {
            input: self.input,
            file_id: self.file_id,
            object: self.object,
            sections: self.sections,
            relocations: self.relocations,
            section_resolutions,
            symbol_id_range,
            section_id_range: self.section_id_range,
            sframe_ranges,
            section_relax_deltas: self.section_relax_deltas,
            thunk_block_id: self.thunk_block_id,
            owns_thunk_block: self.owns_thunk_block,
        })
    }

    pub(crate) fn finalise_symbol<'scope>(
        &self,
        resources: &FinaliseLayoutResources<'scope, 'data, P>,
        flags: ValueFlags,
        local_symbol: &P::SymtabEntry,
        local_symbol_index: object::SymbolIndex,
        section_resolutions: &[SectionResolution],
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut ResolutionWriter<P>,
    ) -> Result {
        let resolution = self.create_symbol_resolution(
            resources,
            flags,
            local_symbol,
            local_symbol_index,
            section_resolutions,
            memory_offsets,
        )?;

        resolutions_out.write(resolution)
    }

    pub(crate) fn create_symbol_resolution<'scope>(
        &self,
        resources: &FinaliseLayoutResources<'scope, 'data, P>,
        flags: ValueFlags,
        local_symbol: &P::SymtabEntry,
        local_symbol_index: object::SymbolIndex,
        section_resolutions: &[SectionResolution],
        memory_offsets: &mut OutputSectionPartMap<u64>,
    ) -> Result<Option<Resolution<P>>> {
        let symbol_id_range = self.symbol_id_range();
        let symbol_id = symbol_id_range.input_to_id(local_symbol_index);

        if !flags.has_resolution() || !resources.symbol_db.is_canonical(symbol_id) {
            return Ok(None);
        }

        let raw_value = if let Some(section_index) = self
            .object
            .symbol_section(local_symbol, local_symbol_index)?
        {
            if let Some(section_address) = section_resolutions[section_index.0].address() {
                let input_offset = self
                    .object
                    .symbol_offset_in_section(local_symbol, section_index)?;
                let output_offset = opt_input_to_output(
                    self.section_relax_deltas.get(section_index.0),
                    input_offset,
                );
                output_offset + section_address
            } else if let Some(x) = get_merged_string_output_address::<P>(
                local_symbol_index,
                0,
                self.object,
                &self.sections,
                &resources.symbol_db.section_part_ids,
                self.section_id_range,
                resources.merged_strings,
                resources.merged_string_start_addresses,
                true,
            )? {
                x
            } else {
                // Don't error for mapping symbols. They cannot have relocations refer to
                // them, so we don't need to produce a resolution.
                if resources.symbol_db.is_mapping_symbol(symbol_id) {
                    return Ok(None);
                }
                bail!(
                    "Symbol is in a section that we didn't load. \
                     Symbol: {} Section: {} Res: {flags}",
                    resources.symbol_debug(symbol_id),
                    section_debug::<P>(self.object, section_index),
                );
            }
        } else if let Some(common) = local_symbol.as_common() {
            let offset = memory_offsets.get_mut(common.part_id);
            let address = *offset;
            *offset += common.size;
            address
        } else {
            local_symbol.value()
        };

        let mut dynamic_symbol_index = None;
        if flags.is_dynamic() {
            // This is an undefined weak symbol. Emit it as a dynamic symbol so that it can be
            // overridden at runtime.
            let dyn_sym_index = P::take_dynsym_index(memory_offsets, resources.section_layouts)?;
            dynamic_symbol_index = Some(
                NonZeroU32::new(dyn_sym_index)
                    .context("Attempted to create dynamic symbol index 0")?,
            );
        }

        Ok(Some(P::create_resolution(
            flags,
            raw_value,
            dynamic_symbol_index,
            memory_offsets,
            resources.symbol_db.args,
            resources.symbol_db.output_kind,
        )))
    }

    pub(crate) fn load_non_hidden_symbols<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        mode: ExportSymbolsMode,
        scope: &Scope<'scope>,
    ) -> Result {
        for (sym_index, sym) in self.object.enumerate_symbols() {
            let symbol_id = self.symbol_id_range().input_to_id(sym_index);

            if let Some(section_index) = self.object.symbol_section(sym, sym_index)?
                && matches!(self.sections[section_index.0], SectionSlot::Discard)
            {
                continue;
            }

            if !can_export_symbol(sym, symbol_id, resources, mode) {
                continue;
            }

            let old_flags = resources
                .per_symbol_flags
                .get_atomic(symbol_id)
                .fetch_or(ValueFlags::EXPORT_DYNAMIC);

            if !old_flags.has_resolution() {
                self.load_symbol::<A>(common, symbol_id, resources, queue, scope)?;
            }

            if !old_flags.needs_export_dynamic() {
                export_dynamic(common, symbol_id, resources.symbol_db)?;
            }
        }
        Ok(())
    }

    pub(crate) fn export_dynamic<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        symbol_id: SymbolId,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        let sym_index = self.symbol_id_range.id_to_input(symbol_id);
        let sym = self.object.symbol(sym_index)?;

        if let Some(section_index) = self.object.symbol_section(sym, sym_index)?
            && matches!(self.sections[section_index.0], SectionSlot::Discard)
        {
            return Ok(());
        }

        // Shared objects that we're linking against sometimes define symbols that are also defined
        // in regular object. When that happens, if we resolve the symbol to the definition from the
        // regular object, then the shared object might send us a request to export the definition
        // provided by the regular object. This isn't always possible, since the symbol might be
        // hidden.
        if !can_export_symbol(sym, symbol_id, resources, ExportSymbolsMode::All) {
            return Ok(());
        }

        let old_flags = resources
            .per_symbol_flags
            .get_atomic(symbol_id)
            .fetch_or(ValueFlags::EXPORT_DYNAMIC);

        if !old_flags.has_resolution() {
            self.load_symbol::<A>(common, symbol_id, resources, queue, scope)?;
        }

        if !old_flags.needs_export_dynamic() {
            export_dynamic(common, symbol_id, resources.symbol_db)?;
        }

        Ok(())
    }

    pub(crate) fn relocations(&self, index: SectionIndex) -> Result<P::RelocationList<'data>> {
        self.object.relocations(index, &self.relocations)
    }

    pub(crate) fn section_part_id(
        &self,
        section_index: SectionIndex,
        global_part_ids: &[PartId],
    ) -> PartId {
        global_part_ids[self.section_id_range.start().as_usize() + section_index.0]
    }
}

pub(crate) struct SymbolCopyInfo<'data> {
    pub(crate) name: &'data [u8],
}

impl<'data> SymbolCopyInfo<'data> {
    /// The primary purpose of this function is to determine whether a symbol should be copied into
    /// the symtab. In the process, we also return the name of the symbol, to avoid needing to read
    /// it again.
    #[inline(always)]
    pub(crate) fn new<P: Platform>(
        object: &P::File<'data>,
        sym_index: object::SymbolIndex,
        sym: &P::SymtabEntry,
        symbol_id: SymbolId,
        symbol_db: &SymbolDb<'data, P>,
        symbol_state: ValueFlags,
        sections: &[SectionSlot],
    ) -> Option<SymbolCopyInfo<'data>> {
        if !symbol_db.is_canonical(symbol_id) || sym.is_undefined() {
            return None;
        }

        if let Ok(Some(section)) = object.symbol_section(sym, sym_index)
            && !sections[section.0].is_loaded()
        {
            // Symbol is in a discarded section.
            return None;
        }

        if sym.as_common().is_some() && !symbol_state.has_resolution() {
            return None;
        }

        // Reading the symbol name is slightly expensive, so we want to do that after all the other
        // checks. That's also the reason why we return the symbol name, so that the caller, if it
        // needs the name, doesn't have a go and read it again.
        let name = object.symbol_name(sym).ok()?;
        if name.is_empty()
            || (!symbol_db.args.should_output_partial_object()
                && !symbol_db.args.discard_none()
                && sym.is_default_strippable(name))
        {
            return None;
        }

        if symbol_db.args.should_strip_symbol_named(name) {
            return None;
        }

        Some(SymbolCopyInfo { name })
    }
}

pub(crate) struct ResolutionWriter<'writer, 'out, P: Platform> {
    pub(crate) resolutions_out: &'writer mut sharded_vec_writer::Shard<'out, Option<Resolution<P>>>,
}

impl<P: Platform> ResolutionWriter<'_, '_, P> {
    pub(crate) fn write(&mut self, res: Option<Resolution<P>>) -> Result {
        self.resolutions_out.try_push(res)?;
        Ok(())
    }
}

impl<'data, P: Platform> StubLibraryLayoutState<'data, P> {
    pub(crate) fn new(stub: &resolution::ResolvedStubLibrary<'data>, args: &P::Args) -> Self {
        Self {
            input: stub.input,
            file_id: stub.file_id,
            symbol_id_range: stub.symbol_id_range,
            format_specific: P::new_stub_library_layout_state_ext(stub, args),
        }
    }

    pub(crate) fn finalise_layout(
        self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut ResolutionWriter<P>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result<FileLayout<'data, P>> {
        Ok(
            match P::finalise_layout_stub(self, memory_offsets, resources, resolutions_out)? {
                Some(format_specific) => {
                    FileLayout::StubLibrary(StubLibraryLayout { format_specific })
                }
                None => FileLayout::NotLoaded,
            },
        )
    }
}

impl<'data, P: Platform> resolution::ResolvedFile<'data, P> {
    pub(crate) fn create_layout_state(self, args: &P::Args) -> FileLayoutState<'data, P> {
        match self {
            resolution::ResolvedFile::Object(s) => new_object_layout_state(s),
            resolution::ResolvedFile::Dynamic(s) => new_dynamic_object_layout_state(&s, args),
            resolution::ResolvedFile::StubLibrary(s) => {
                FileLayoutState::StubLibrary(StubLibraryLayoutState::new(&s, args))
            }
            resolution::ResolvedFile::Prelude(s) => {
                FileLayoutState::Prelude(PreludeLayoutState::new(s, args))
            }
            resolution::ResolvedFile::NotLoaded(s) => FileLayoutState::NotLoaded(s),
            resolution::ResolvedFile::LinkerScript(s) => {
                FileLayoutState::LinkerScript(LinkerScriptLayoutState::new(s))
            }
            resolution::ResolvedFile::SyntheticSymbols(s) => {
                FileLayoutState::SyntheticSymbols(SyntheticSymbolsLayoutState::new(s))
            }
            #[cfg(all(feature = "plugins", unix))]
            resolution::ResolvedFile::LtoInput(s) => FileLayoutState::NotLoaded(NotLoaded {
                symbol_id_range: s.symbol_id_range,
                section_id_range: s.section_id_range,
            }),
        }
    }
}

impl<P: Platform> Resolution<P> {
    pub(crate) fn flags(self) -> ValueFlags {
        self.flags
    }

    pub(crate) fn value(self) -> u64 {
        self.raw_value
    }

    pub(crate) fn address(&self) -> Result<u64> {
        if !self.flags.has_link_time_address() {
            bail!("Expected address, found {}", self.flags);
        }
        Ok(self.raw_value)
    }

    pub(crate) fn value_for_symbol_table(&self) -> u64 {
        self.raw_value
    }

    pub(crate) fn is_absolute(&self) -> bool {
        self.flags.is_absolute()
    }

    pub(crate) fn dynamic_symbol_index(&self) -> Result<u32> {
        Ok(self
            .dynamic_symbol_index
            .context("Missing dynamic_symbol_index")?
            .get())
    }
}

/// Maximum number of relaxation scan iterations. In practice convergence
/// happens in 2–3 passes.
pub(crate) const MAX_RELAXATION_ITERATIONS: usize = 5;

/// Sentinel value stored in `SymbolOutputInfos::addresses` for symbols whose output address is
/// unknown.
pub(crate) const SYMBOL_ADDRESS_UNRESOLVED: u64 = u64::MAX;

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputSectionPosition {
    pub(crate) part_id: PartId,
    pub(crate) address: u64,
}

/// Input-section positions in the coordinate system supplied by the initial part offsets.
/// Zero initial offsets produce part-relative positions, while final part offsets produce output
/// addresses.
pub(crate) type InputSectionPositions = Vec<Vec<Vec<Option<InputSectionPosition>>>>;

/// Stores precomputed output-address information for every symbol.
pub(crate) struct SymbolOutputInfos {
    pub(crate) addresses: Vec<u64>,
}

impl SymbolOutputInfos {
    pub(crate) fn resolve(
        &self,
        symbol_id: SymbolId,
        per_symbol_flags: &PerSymbolFlags,
    ) -> Option<RelaxSymbolInfo> {
        let addr = *self.addresses.get(symbol_id.as_usize())?;
        if addr == SYMBOL_ADDRESS_UNRESOLVED {
            return None;
        }
        Some(RelaxSymbolInfo {
            output_address: addr,
            is_interposable: per_symbol_flags
                .flags_for_symbol(symbol_id)
                .is_interposable(),
        })
    }
}

pub(crate) enum EarlyObjectSymbolValue {
    Absolute(u64),
    PartRelative { part_id: PartId, offset: u64 },
}

/// Per-file list of section indices to rescan on subsequent relaxation iterations. Indexed as
/// `[group_idx][file_idx]`.  Files that are not objects get an empty entry.
pub(crate) type RescanSections = Vec<Vec<SmallVec<[usize; 16]>>>;

/// Like `RescanSections` but each entry also carries the minimum margin (in bytes) among the
/// section's unrelaxed candidates.  This is returned by `relaxation_scan_pass` and then filtered
/// by `total_deleted` to produce a `RescanSections` for the next iteration.
pub(crate) type RescanCandidates = Vec<Vec<SmallVec<[(usize, u64); 16]>>>;

pub(crate) struct InputOrderItem {
    pub(crate) part_id: PartId,
    pub(crate) group_idx: usize,
    pub(crate) link_order: u32,
    pub(crate) alignment: Alignment,
    pub(crate) size: u64,
}

pub(crate) fn object_symbol_address_in_layout<'data, P: Platform>(
    name: &[u8],
    obj: &SequencedInputObject<'data, P>,
    definition: SymbolId,
    symbol_db: &SymbolDb<'data, P>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
) -> Result<u64> {
    let local_index = definition.to_input(obj.symbol_id_range);
    let symbol = obj.parsed.object.symbol(local_index)?;
    if symbol.is_absolute() {
        return Ok(symbol.value());
    }

    let Some(section_index) = obj.parsed.object.symbol_section(symbol, local_index)? else {
        return Ok(symbol.value());
    };

    let offset = obj
        .parsed
        .object
        .symbol_offset_in_section(symbol, section_index)?;
    let part_id = symbol_db.part_id_for_symbol(definition);
    if part_id == crate::part_id::UNMAPPED {
        bail!(
            "symbol `{}` is not in an output section",
            String::from_utf8_lossy(name)
        );
    }

    let output_id = part_id.output_section_id::<P>();
    let layout = section_layouts.get(output_id);
    if layout.mem_size == 0 && layout.mem_offset == 0 && layout.file_offset == 0 {
        bail!(
            "symbol `{}` is used in a location-counter assignment before its section has been laid out",
            String::from_utf8_lossy(name)
        );
    }

    Ok(layout.mem_offset + offset)
}

/// Computes the maximum alignment for each LOAD segment by examining the alignments of all sections
/// that will be placed in that segment.
pub(crate) fn compute_segment_alignments<'data, P: Platform>(
    sizes: &OutputSectionPartMap<u64>,
    program_segments: &ProgramSegments<P::ProgramSegmentDef>,
    output_order: &OutputOrder<'data>,
    args: &P::Args,
    output_sections: &OutputSections<P>,
) -> HashMap<ProgramSegmentId, Alignment> {
    timing_phase!("Computing segment alignments");

    let mut segment_alignments: HashMap<ProgramSegmentId, Alignment> = HashMap::new();
    let mut active_load_segments: Vec<ProgramSegmentId> = Vec::new();

    for event in output_order {
        match event {
            OrderEvent::SegmentStart(segment_id) => {
                if program_segments.is_load_segment(segment_id) {
                    // Initialize with the base loadable segment alignment
                    segment_alignments
                        .entry(segment_id)
                        .or_insert_with(|| args.loadable_segment_alignment());
                    active_load_segments.push(segment_id);
                }
            }
            OrderEvent::SegmentEnd(segment_id) => {
                active_load_segments.retain(|&id| id != segment_id);
            }
            OrderEvent::Section(section_id) => {
                let part_id_range = section_id.part_id_range::<P>();
                let max_alignment = sizes.max_alignment(part_id_range, output_sections);

                // Update the alignment for all active LOAD segments
                for &segment_id in &active_load_segments {
                    segment_alignments
                        .entry(segment_id)
                        .and_modify(|a| *a = (*a).max(max_alignment));
                }
            }
            OrderEvent::SetLocation(..)
            | OrderEvent::SetLocationRelative(..)
            | OrderEvent::SetSectionAddress(_) => {}
        }
    }

    segment_alignments
}

impl<'data, P: Platform> DynamicLayoutState<'data, P> {
    pub(crate) fn activate<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, '_, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        P::activate_dynamic(self, common);

        self.request_all_undefined_symbols::<A>(resources, queue, scope)
    }

    pub(crate) fn request_all_undefined_symbols<'scope, A: Arch<Platform = P>>(
        &self,
        resources: &'scope GraphResources<'data, '_, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        let mut check_undefined_cache = None;

        for symbol_id in self.symbol_id_range() {
            let definition_symbol_id = resources.symbol_db.definition(symbol_id);

            let flags = resources.local_flags_for_symbol(definition_symbol_id);

            if flags.is_dynamic() && flags.is_absolute() {
                // Our shared object references an undefined symbol. Whether that is an error or
                // not, depends on flags, whether the symbol is weak and whether all of the shared
                // object's dependencies are loaded.

                let args = resources.symbol_db.args;
                let check_undefined = *check_undefined_cache
                    .get_or_insert_with(|| self.object.should_enforce_undefined(resources));

                if check_undefined {
                    let symbol = self
                        .object
                        .symbol(self.symbol_id_range.id_to_input(symbol_id))?;
                    if !symbol.is_weak() {
                        let should_report = !matches!(
                            args.unresolved_symbols_behaviour(),
                            crate::args::UnresolvedSymbols::IgnoreAll
                                | crate::args::UnresolvedSymbols::IgnoreInSharedLibs
                        );

                        if should_report {
                            let symbol_name =
                                resources.symbol_db.symbol_name_for_display(symbol_id);

                            if args.should_error_on_unresolved_symbols() {
                                bail!("undefined reference to `{symbol_name}` from {self}");
                            }
                            resources.symbol_db.warning(format!(
                                "undefined reference to `{symbol_name}` from {self}"
                            ));
                        }
                    }
                }
            } else if definition_symbol_id != symbol_id {
                let file_id = resources.symbol_db.file_id_for_symbol(definition_symbol_id);

                queue.send_work::<A>(
                    resources,
                    file_id,
                    WorkItem::ExportDynamic(definition_symbol_id),
                    scope,
                );
            }
        }

        Ok(())
    }

    pub(crate) fn finalise_sizes(&mut self, common: &mut CommonGroupState<'data, P>) -> Result {
        P::finalise_sizes_dynamic(self, common)?;

        self.object.finalise_sizes_dynamic(
            self.lib_name,
            &mut self.format_specific,
            &mut common.mem_sizes,
        )?;

        Ok(())
    }

    pub(crate) fn finalise_layout(
        mut self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut ResolutionWriter<P>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result<FileLayout<'data, P>> {
        let file_id = self.file_id();

        Ok(
            match P::finalise_layout_dynamic(&mut self, memory_offsets, resources, resolutions_out)?
            {
                Some(format_specific) => FileLayout::Dynamic(DynamicLayout {
                    file_id,
                    input: self.input,
                    lib_name: self.lib_name,
                    object: self.object,
                    symbol_id_range: self.symbol_id_range,
                    format_specific,
                }),
                None => FileLayout::NotLoaded,
            },
        )
    }
}

impl<'data, P: Platform> LinkerScriptLayoutState<'data, P> {
    pub(crate) fn finalise_layout(
        &self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &mut ResolutionWriter<P>,
        resources: &FinaliseLayoutResources<'_, 'data, P>,
    ) -> Result {
        self.internal_symbols
            .finalise_layout(memory_offsets, resolutions_out, resources)
    }

    pub(crate) fn new(input: resolution::ResolvedLinkerScript<'data, P>) -> Self {
        Self {
            file_id: input.file_id,
            input: input.input,
            symbol_id_range: input.symbol_id_range,
            internal_symbols: InternalSymbols {
                symbol_definitions: input.symbol_definitions,
                start_symbol_id: input.symbol_id_range.start(),
            },
        }
    }

    pub(crate) fn activate<'scope, A: Arch<Platform = P>>(
        &self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, '_, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        for group in &resources.symbol_db.groups {
            match group {
                Group::LinkerScripts(linker_scripts) => {
                    for script in linker_scripts {
                        for lc in &script.parsed.location_counters {
                            load_expression_referenced_symbols::<A>(
                                resources,
                                queue,
                                scope,
                                lc.get_expression(),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        self.internal_symbols
            .activate_symbols::<A>(common, resources, queue, scope)
    }

    pub(crate) fn finalise_sizes(
        &self,
        common: &mut CommonGroupState<'data, P>,
        per_symbol_flags: &AtomicPerSymbolFlags,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        self.internal_symbols.allocate_symbol_table_sizes(
            &mut common.mem_sizes,
            resources.symbol_db,
            |symbol_id, _info| {
                per_symbol_flags
                    .flags_for_symbol(symbol_id)
                    .has_resolution()
            },
        )?;

        Ok(())
    }
}

impl<'data, P: Platform> Layout<'data, P> {
    pub(crate) fn mem_address_of_built_in(&self, section_id: OutputSectionId) -> u64 {
        self.section_layouts.get(section_id).mem_offset
    }
}

impl<'data, P: Platform> std::fmt::Debug for FileLayoutState<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileLayoutState::Object(s) => f.debug_tuple("Object").field(&s.input).finish(),
            FileLayoutState::Prelude(_) => f.debug_tuple("Internal").finish(),
            FileLayoutState::Dynamic(s) => f.debug_tuple("Dynamic").field(&s.input).finish(),
            FileLayoutState::StubLibrary(s) => {
                f.debug_tuple("StubLibrary").field(&s.input).finish()
            }
            FileLayoutState::LinkerScript(s) => {
                f.debug_tuple("LinkerScript").field(&s.input).finish()
            }
            FileLayoutState::NotLoaded(_) => Display::fmt(&"<not loaded>", f),
            FileLayoutState::Epilogue(_) => Display::fmt(&"<custom sections>", f),
            FileLayoutState::SyntheticSymbols(_) => Display::fmt(&"<synthetic symbols>", f),
        }
    }
}

impl<P: Platform> GcLoadRequest<P> {
    pub(crate) fn new(file_id: FileId, gc_unit: P::GcUnit) -> Self {
        Self { file_id, gc_unit }
    }
}

impl<'data, P: Platform> ObjectLayout<'data, P> {
    pub(crate) fn relocations(&self, index: SectionIndex) -> Result<P::RelocationList<'data>> {
        self.object.relocations(index, &self.relocations)
    }

    pub(crate) fn section_part_id(
        &self,
        section_index: SectionIndex,
        part_ids: &[PartId],
    ) -> PartId {
        part_ids[self.section_id_range.input_to_id(section_index).as_usize()]
    }
}

impl<'scope, 'data, P: Platform> FinaliseLayoutResources<'scope, 'data, P> {
    pub(crate) fn symbol_debug<'a>(&'a self, symbol_id: SymbolId) -> SymbolDebug<'a, 'data, P> {
        self.symbol_db
            .symbol_debug(self.per_symbol_flags, symbol_id)
    }
}

impl OutputRecordLayout {
    pub(crate) fn file_end(&self) -> usize {
        self.file_offset + self.file_size
    }

    pub(crate) fn mem_end(&self) -> u64 {
        self.mem_offset + self.mem_size
    }

    pub(crate) fn merge(&mut self, other: &OutputRecordLayout) {
        debug_assert!(other.mem_offset >= self.mem_offset);
        debug_assert!(other.file_offset >= self.file_offset);
        self.mem_size += other.mem_size;
        self.file_size += other.file_size;
        if other.mem_size > 0 {
            self.alignment = self.alignment.max(other.alignment);
        }
    }
}

// This implementation is just here so that we can store a Box<dyn Drop> elsewhere in order to erase
// the type parameter P, allowing deferred dropping to occur.
impl<'data, P: Platform> Drop for Layout<'data, P> {
    fn drop(&mut self) {}
}

/// A GC unit for use on platform where GC is done by section. Effectively an object::SectionIndex,
/// but stored as a u32 for compactness.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SectionGcUnit(u32);

impl SectionGcUnit {
    pub(crate) fn new(section_index: object::SectionIndex) -> Self {
        Self(section_index.0 as u32)
    }

    pub(crate) fn section_index(self) -> object::SectionIndex {
        object::SectionIndex(self.0 as usize)
    }
}

/// An input section that needs to be sorted due to a SORT_BY_NAME directive or equivalent.
#[derive(Copy, Clone, Debug)]
pub(crate) struct InputSortedSection {
    pub(crate) file_id: FileId,
    pub(crate) section_index: object::SectionIndex,
    pub(crate) part_id: PartId,
    pub(crate) size: u64,
    pub(crate) alignment: Alignment,
}

pub(crate) fn assign_addresses_to_sorted_sections<P: Platform>(
    group_states: &mut [GroupState<P>],
    starting_mem_offsets_by_group: &[OutputSectionPartMap<u64>],
    sorted_sections: &mut [InputSortedSection],
) {
    let mut epilogue_offsets = starting_mem_offsets_by_group.last().unwrap().clone();

    for sec in sorted_sections {
        let offset = epilogue_offsets.get_mut(sec.part_id);
        *offset = sec.alignment.align_up(*offset);

        let FileLayoutState::Object(obj) =
            &mut group_states[sec.file_id.group()].files[sec.file_id.file()]
        else {
            unreachable!();
        };

        let SectionSlot::Sorted(slot) = &mut obj.sections[sec.section_index.0] else {
            unreachable!();
        };

        slot.address = *offset;
        *offset += sec.size;
    }
}
