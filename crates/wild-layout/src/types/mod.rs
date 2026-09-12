use crate::{new_dynamic_object_layout_state, new_object_layout_state};
mod gc;
mod objects;
mod units;

use crate::expression_eval::ResolvedLocationCounter;
use crate::grouping::SequencedInputObject;
use crate::output_section_id::{OrderEvent, OutputOrder, OutputSectionId, OutputSections};
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::InternalSymDefInfo;
use crate::part_id::PartId;
use crate::resolution::{NotLoaded, ResolvedGroup, ScriptSortedSectionDetail, SectionSlot};
use crate::string_merging::{MergedStringStartAddresses, MergedStringsSection};
use crate::symbol_db::{SymbolDb, SymbolDebug, SymbolId, SymbolIdRange};
use crate::thunks::{ThunkBlockId, ThunkLayoutBuilder};
use crate::{EnginePlatform, resolution, timing_phase};
#[allow(unused_imports)]
pub use gc::*;
use hashbrown::{HashMap, HashSet};
use linker_utils::relaxation::RelaxDeltaMap;
use object::SectionIndex;
#[allow(unused_imports)]
pub use objects::*;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::mem::replace;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64};
#[allow(unused_imports)]
pub use units::*;
use wild_args::InputRef;
use wild_error::bail;
use wild_error::error::{Context, Error, Result};
use wild_platform::output_section_map::OutputSectionMap;
use wild_platform::program_segments::{ProgramSegmentId, ProgramSegments};
use wild_platform::value_flags::{
    AtomicPerSymbolFlags, FlagsForSymbol as _, PerSymbolFlags, ValueFlags,
};
use wild_platform::{
    Args as _, FileId, ObjectFile, Platform, RelaxSymbolInfo, SectionAttributes as _,
    SectionFlags as _, Symbol as _,
};
use wild_util::alignment::Alignment;
use wild_util::input_section_id::SectionIdRange;

pub struct FinaliseSizesResources<'data, 'scope, P: Platform> {
    pub dynamic_symbol_definitions: &'scope [DynamicSymbolDefinition<'data, P>],
    pub symbol_db: &'scope SymbolDb<'data, P>,
    pub merged_strings: &'scope OutputSectionMap<MergedStringsSection<'data>>,
    pub format_specific: &'scope P::FinaliseSizesExt<'data>,
    pub script_sorted_sections: &'scope [InputSortedSection],
}

/// Compressed debug-section payload stored on [`Layout`] for later writing.
#[derive(Debug)]
pub struct CompressedSection {
    pub compressed_chunks: Vec<Vec<u8>>,
    pub total_compressed_size: usize,
}

/// Information about what goes where. Also includes relocation data, since that's computed at the
/// same time.
#[derive(Debug)]
pub struct Layout<'data, P: Platform> {
    pub symbol_db: SymbolDb<'data, P>,
    pub symbol_resolutions: SymbolResolutions<P>,
    pub got_relr_n: u64,
    pub section_part_layouts: OutputSectionPartMap<OutputRecordLayout>,

    pub section_layouts: OutputSectionMap<OutputRecordLayout>,

    /// This is like `section_layouts`, but where secondary sections are merged into their primary
    /// section. Values for secondary sections are reset to 0 and should not be used.
    pub merged_section_layouts: OutputSectionMap<OutputRecordLayout>,

    pub group_layouts: Vec<GroupLayout<'data, P>>,
    pub segment_layouts: SegmentLayouts,
    pub output_sections: OutputSections<'data, P>,
    pub program_segments: ProgramSegments<P::ProgramSegmentDef>,
    pub output_order: OutputOrder<'data>,
    pub non_addressable_counts: P::NonAddressableCounts,
    pub merged_strings: OutputSectionMap<MergedStringsSection<'data>>,
    pub merged_string_start_addresses: MergedStringStartAddresses,
    pub relocation_statistics: OutputSectionMap<AtomicU64>,
    pub has_static_tls: bool,
    pub has_variant_pcs: bool,
    pub per_symbol_flags: PerSymbolFlags,
    pub dynamic_symbol_definitions: Vec<DynamicSymbolDefinition<'data, P>>,
    pub format_specific: P::LayoutExt<'data>,
    /// Thunk address maps indexed by ThunkBlockId. Each entry maps SymbolId to the memory address
    /// of the thunk for that symbol within the block.
    pub thunk_block_addresses: Vec<BTreeMap<SymbolId, u64>>,

    pub compressed_debug_sections: OutputSectionMap<Option<CompressedSection>>,
    pub gdb_index_data: Option<P::GdbIndexScanResult<'data>>,
    pub script_sorted_sections: Vec<InputSortedSection>,
    pub resolved_location_counters: Vec<ResolvedLocationCounter>,
    /// Object FileIds whose allocatable section payloads can be left in the existing output during
    /// an incremental update. Empty unless `--incremental` is doing an in-place rewrite.
    pub incremental_skip_payloads: HashSet<FileId>,
    /// This-run `FileId` → generational atom. Empty unless `--incremental`.
    pub incremental_atoms: HashMap<FileId, crate::incremental::AtomId>,
    /// Sites that applied a relocation, keyed by defined atom + local symbol. Empty when not
    /// incremental.
    pub incremental_reverse_relocs: Mutex<crate::incremental::ReverseRelocIndex>,
    /// Loaded previous reverse-reloc index + resolutions for patching skipped objects.
    pub incremental_patch: Option<crate::incremental::IncrementalPatchJob>,

    pub partial_link: PartialLinkSingletons,
}

#[derive(Debug, Default)]
pub struct PartialLinkSingletons {
    output_indexes: std::ops::Range<u32>,
    groups: Vec<SingletonGroupPlan>,
}

#[derive(Debug)]
pub struct PartialLinkPlan {
    pub groups: Vec<SingletonGroupPlan>,
    pub singleton_count: u32,
    pub section_name_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct SingletonGroupPlan {
    pub ordinals: std::ops::Range<u32>,
    pub section_name_bytes: u64,
}

impl PartialLinkPlan {
    pub fn build<P: EnginePlatform>(
        group_states: &[GroupState<P>],
        section_part_ids: &[PartId],
        args: &P::Args,
    ) -> Result<Option<Self>> {
        if !args.should_output_partial_object() {
            return Ok(None);
        }

        let singleton_id = P::PARTIAL_SINGLETONS_ID
            .context("Partial linking is not supported for this output format")?;

        let group_counts = group_states
            .par_iter()
            .map(|group| {
                let mut singleton_count = 0u32;
                let mut section_name_bytes = 0u64;

                for file in &group.files {
                    let FileLayoutState::Object(object) = file else {
                        continue;
                    };

                    let object_part_ids = &section_part_ids[object.section_id_range.as_usize()];

                    for (raw_index, (slot, &part_id)) in
                        object.sections.iter().zip(object_part_ids).enumerate()
                    {
                        if part_id.output_section_id::<P>() != singleton_id {
                            continue;
                        }
                        // Partial-link custom sections use ordinary loading, including debug sections.
                        match slot {
                            SectionSlot::Loaded(_) => {}
                            SectionSlot::Discard | SectionSlot::Unloaded(_) => continue,
                            _ => bail!(
                                "Internal error: partial-link singleton {} in {} has unexpected state {slot:?}",
                                object.object.section_display_name(SectionIndex(raw_index)),
                                object.input,
                            ),
                        }

                        singleton_count = singleton_count
                            .checked_add(1)
                            .context("Too many partial-link singleton sections")?;
                        section_name_bytes +=
                            object.object.section_name(SectionIndex(raw_index))?.len() as u64 + 1;
                    }
                }

                Ok((singleton_count, section_name_bytes))
            })
            .collect::<Result<Vec<_>>>()?;

        let mut singleton_count = 0u32;
        let mut section_name_bytes = 0u64;
        let groups = group_counts
            .into_iter()
            .map(|(count, name_bytes)| {
                let start = singleton_count;
                singleton_count = singleton_count
                    .checked_add(count)
                    .context("Too many partial-link singleton sections")?;
                section_name_bytes += name_bytes;
                Ok(SingletonGroupPlan {
                    ordinals: start..singleton_count,
                    section_name_bytes: name_bytes,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Some(Self {
            groups,
            singleton_count,
            section_name_bytes,
        }))
    }
}

impl PartialLinkSingletons {
    pub fn group_sizes(&self) -> impl Iterator<Item = (usize, usize)> {
        self.groups
            .iter()
            .map(|group| (group.ordinals.len(), group.section_name_bytes as usize))
    }

    pub fn is_empty(&self) -> bool {
        self.output_indexes.is_empty()
    }

    pub fn output_index_range(&self) -> std::ops::Range<u32> {
        self.output_indexes.clone()
    }

    pub fn output_index(&self, singleton: &PartialLinkSingleton) -> u32 {
        self.output_indexes.start + singleton.ordinal
    }
}

impl SingletonGroupPlan {
    pub fn finalise<P: EnginePlatform>(
        &self,
        layout: &mut GroupLayout<P>,
        section_part_ids: &[PartId],
    ) {
        if self.ordinals.is_empty() {
            return;
        }
        let singleton_id = P::PARTIAL_SINGLETONS_ID.unwrap();
        let mut ordinal = self.ordinals.start;
        for file in &mut layout.files {
            let FileLayout::Object(object) = file else {
                continue;
            };
            let part_ids = &section_part_ids[object.section_id_range.as_usize()];
            for (slot, &part_id) in object.sections.iter_mut().zip(part_ids) {
                if part_id.output_section_id::<P>() != singleton_id {
                    continue;
                }
                let section = match *slot {
                    SectionSlot::Loaded(section) => section,
                    SectionSlot::Discard | SectionSlot::Unloaded(_) => continue,
                    _ => unreachable!("Singleton section changed state after planning: {slot:?}"),
                };
                *slot =
                    SectionSlot::PartialLinkSingleton(PartialLinkSingleton { section, ordinal });
                ordinal += 1;
            }
        }
        assert_eq!(ordinal, self.ordinals.end);
    }
}

#[derive(Debug, Default)]
pub struct SegmentLayouts {
    /// The layout of each of our segments. Segments containing no active output sections will have
    /// been filtered, so don't try to index this by our internal segment IDs.
    pub segments: Vec<SegmentLayout>,
    pub tls_layout: Option<OutputRecordLayout>,
}

#[derive(Debug, Default, Clone)]
pub struct SegmentLayout {
    pub id: ProgramSegmentId,
    pub sizes: OutputRecordLayout,
}

#[derive(Debug)]
pub struct SymbolResolutions<P: Platform> {
    pub resolutions: Vec<Option<Resolution<P>>>,
}

impl<P: EnginePlatform> SymbolResolutions<P> {
    pub fn get(&self, symbol_id: SymbolId) -> Option<&Resolution<P>> {
        self.resolutions[symbol_id.as_usize()].as_ref()
    }

    pub fn raw_values(&self) -> impl Iterator<Item = u64> + '_ {
        self.resolutions
            .iter()
            .map(|r| r.as_ref().map(|res| res.raw_value).unwrap_or(0))
    }
}

pub enum FileLayout<'data, P: Platform> {
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
pub struct Resolution<P: Platform> {
    /// An address or absolute value.
    pub raw_value: u64,

    pub dynamic_symbol_index: Option<NonZeroU32>,

    pub flags: ValueFlags,

    pub format_specific: P::ResolutionExt,
}

/// Address information for a section.
#[derive(derive_more::Debug, Clone, Copy, Eq, PartialEq)]
pub struct SectionResolution {
    #[debug("0x{address:x}")]
    pub address: u64,
}

impl SectionResolution {
    /// Returns a resolution for a section that we didn't load, or for which we don't have an
    /// address (e.g. string-merge sections).
    pub fn none() -> SectionResolution {
        SectionResolution { address: u64::MAX }
    }

    pub fn address(self) -> Option<u64> {
        if self.address == u64::MAX {
            None
        } else {
            Some(self.address)
        }
    }

    /// Converts to a resolution compatible with what's used for symbols.
    pub fn full_resolution<P: EnginePlatform>(self) -> Option<Resolution<P>> {
        let address = self.address()?;
        Some(Resolution {
            raw_value: address,
            dynamic_symbol_index: None,
            flags: ValueFlags::empty(),
            format_specific: Default::default(),
        })
    }
}

pub enum FileLayoutState<'data, P: Platform> {
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
pub struct PreludeLayoutState<'data, P: Platform> {
    pub file_id: FileId,
    pub symbol_id_range: SymbolIdRange,
    pub internal_symbols: InternalSymbols<'data, P>,
    pub entry_symbol_id: Option<SymbolId>,
    pub identity: String,
    pub header_info: Option<HeaderInfo>,
    pub dynamic_linker: Option<CString>,
    pub format_specific: P::PreludeLayoutStateExt,
}

pub struct SyntheticSymbolsLayoutState<'data, P: Platform> {
    pub file_id: FileId,
    pub symbol_id_range: SymbolIdRange,
    pub internal_symbols: InternalSymbols<'data, P>,
    pub start_stop_sections: Option<OutputSectionMap<Vec<resolution::StartStopCandidate<P>>>>,
}

pub struct EpilogueLayoutState<P: Platform> {
    pub format_specific: P::EpilogueLayoutExt,
}

#[derive(Debug)]
pub struct StubLibraryLayoutState<'data, P: Platform> {
    pub input: InputRef<'data>,
    pub file_id: FileId,
    pub symbol_id_range: SymbolIdRange,
    pub format_specific: P::StubLibraryLayoutStateExt,
}

#[derive(Debug)]
pub struct StubLibraryLayout<P: Platform> {
    pub format_specific: P::StubLibraryLayoutExt,
}

#[derive(Debug)]
pub struct LinkerScriptLayoutState<'data, P: Platform> {
    pub file_id: FileId,
    pub input: InputRef<'data>,
    pub symbol_id_range: SymbolIdRange,
    pub internal_symbols: InternalSymbols<'data, P>,
}

#[derive(Debug)]
pub struct SyntheticSymbolsLayout<'data, P: Platform> {
    pub internal_symbols: InternalSymbols<'data, P>,
}

#[derive(Debug)]
pub struct EpilogueLayout<P: Platform> {
    pub format_specific: P::EpilogueLayoutExt,
    pub dynsym_start_index: u32,
}

#[derive(Debug)]
pub struct ObjectLayout<'data, P: Platform> {
    pub input: InputRef<'data>,
    pub file_id: FileId,
    pub object: &'data P::File<'data>,
    pub sections: Vec<SectionSlot>,
    pub relocations: P::RelocationSections,
    pub section_resolutions: Vec<SectionResolution>,
    pub symbol_id_range: SymbolIdRange,
    pub section_id_range: SectionIdRange,

    /// SFrame section ranges for this object, relative to the start of the .sframe output section.
    pub sframe_ranges: Vec<std::ops::Range<usize>>,

    /// Sparse map from section index to relaxation delta details.
    pub section_relax_deltas: RelaxDeltaMap,

    /// Which ThunkBlock holds primary thunks for this object. Used during relocation writing to
    /// look up the thunk address for out-of-range branch targets.
    pub thunk_block_id: crate::thunks::ThunkBlockId,

    /// Whether this object is responsible for writing the thunks in its ThunkBlock.
    pub owns_thunk_block: bool,
}

#[derive(Debug)]
pub struct PreludeLayout<'data, P: Platform> {
    pub entry_symbol_id: Option<SymbolId>,
    pub identity: String,
    pub header_info: HeaderInfo,
    pub internal_symbols: InternalSymbols<'data, P>,
    pub dynamic_linker: Option<CString>,
    pub format_specific: P::PreludeLayoutExt,
}

#[derive(Debug)]
pub struct InternalSymbols<'data, P: Platform> {
    pub symbol_definitions: Vec<InternalSymDefInfo<'data, P>>,
    pub start_symbol_id: SymbolId,
}

#[derive(Debug)]
pub struct DynamicLayout<'data, P: Platform> {
    pub file_id: FileId,
    pub input: InputRef<'data>,

    /// The name we'll put into the binary to tell the dynamic loader what to load.
    pub lib_name: &'data [u8],

    pub symbol_id_range: SymbolIdRange,

    pub object: &'data P::File<'data>,

    pub format_specific: P::DynamicLayoutExt<'data>,
}

#[derive(Debug)]
pub struct CommonGroupState<'data, P: Platform> {
    pub mem_sizes: OutputSectionPartMap<u64>,

    pub section_attributes: HashMap<OutputSectionId, P::SectionAttributes>,

    /// Dynamic symbols that need to be defined. Because of the ordering requirements for symbol
    /// hashes, these get defined by the epilogue. The object on which a particular dynamic symbol
    /// is stored is non-deterministic and is whichever object first requested export of that
    /// symbol. That's OK though because the epilogue will sort all dynamic symbols.
    pub dynamic_symbol_definitions: Vec<DynamicSymbolDefinition<'data, P>>,

    pub format_specific: P::CommonGroupStateExt,
}

pub struct ObjectLayoutState<'data, P: Platform> {
    pub input: InputRef<'data>,
    pub file_id: FileId,
    pub symbol_id_range: SymbolIdRange,
    pub section_id_range: SectionIdRange,
    pub object: &'data P::File<'data>,

    /// Command-line section concatenation order. Plugin codegen shares the first LTO input's
    /// position (#1935).
    pub link_order: u32,

    /// Info about each of our sections. Indexed the same as the sections in the input object.
    pub sections: Vec<SectionSlot>,

    /// Mapping from sections to their corresponding relocation section.
    pub relocations: P::RelocationSections,

    pub format_specific: P::ObjectLayoutStateExt<'data>,

    /// Sparse map from section index to relaxation delta details, built during `finalise_sizes`
    /// and later transferred to `ObjectLayout`.
    pub section_relax_deltas: RelaxDeltaMap,

    pub script_sorted_sections: Vec<ScriptSortedSectionDetail>,

    /// Which ThunkBlock handles primary-part thunks for this object.
    pub thunk_block_id: ThunkBlockId,

    /// Whether this object is responsible for writing the thunk block.
    pub owns_thunk_block: bool,

    /// Total bytes of primary-function-part sections that survived GC. Used to help determine
    /// distances for range-extension thunks.
    pub post_gc_primary_bytes: u64,
}

#[derive(Debug, Default)]
pub struct LocalWorkQueue<P: Platform> {
    /// The index of the worker that owns this queue.
    pub index: usize,

    /// Work that needs to be processed by the worker that owns this queue.
    pub local_work: Vec<WorkItem<P>>,
}

pub struct DynamicLayoutState<'data, P: Platform> {
    pub object: &'data P::File<'data>,
    pub input: InputRef<'data>,
    pub file_id: FileId,
    pub symbol_id_range: SymbolIdRange,
    pub lib_name: &'data [u8],

    pub format_specific: P::DynamicLayoutStateExt<'data>,
}

#[derive(derive_more::Debug, Clone, Copy)]
pub struct DynamicSymbolDefinition<'data, P: Platform> {
    pub symbol_id: SymbolId,
    #[debug("{:?}", String::from_utf8_lossy(name))]
    pub name: &'data [u8],
    pub format_specific: P::DynamicSymbolDefinitionExt,
}

#[derive(Debug, Clone, Copy)]
pub struct Section {
    /// Size in the output. This starts as the input section size, then may be reduced by
    /// relaxation-induced byte deletions during `scan_relaxations`.
    pub size: u64,
    pub alignment: Alignment,
}

#[derive(Debug, Clone, Copy)]
pub struct SortedSection {
    pub address: u64,
    pub section: Section,
}

/// A section with a unique name that is passed through from input to output without merging
/// with other input sections. Created after group layout finalisation.
#[derive(Debug, Clone, Copy)]
pub struct PartialLinkSingleton {
    pub section: Section,
    pub ordinal: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SectionGroupOrder {
    Prelude,
    Object(u32),
    Other,
    Epilogue,
}

pub fn section_group_order<P: EnginePlatform>(files: &[FileLayoutState<P>]) -> SectionGroupOrder {
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
pub struct GroupLayout<'data, P: Platform> {
    pub files: Vec<FileLayout<'data, P>>,

    /// The offset in .dynstr at which we'll start writing.
    pub dynstr_start_offset: u32,

    /// The offset in .strtab at which we'll start writing.
    pub strtab_start_offset: u32,

    pub symtab_local_start_index: u32,
    pub symtab_global_start_index: u32,

    pub mem_sizes: OutputSectionPartMap<u64>,
    pub file_sizes: OutputSectionPartMap<usize>,

    pub format_specific: P::GroupLayoutExt,

    pub section_group_order: SectionGroupOrder,
}

#[derive(Debug)]
pub struct GroupState<'data, P: Platform> {
    pub queue: LocalWorkQueue<P>,
    pub files: Vec<FileLayoutState<'data, P>>,
    pub common: CommonGroupState<'data, P>,
    pub num_symbols: usize,
    pub section_group_order: SectionGroupOrder,
}

/// The sizes and positions of either a segment or an output section. Note, we use usize for file
/// offsets and sizes, since we mmap our output file, so we're frequently working with in-memory
/// slices. This means that if we were linking on a 32 bit system that we'd be limited to file
/// offsets that were 32 bits. This isn't a loss though, since we couldn't mmap an output file where
/// that would be a problem on a 32 bit system.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OutputRecordLayout {
    pub file_size: usize,
    pub mem_size: u64,
    pub alignment: Alignment,
    pub file_offset: usize,
    pub mem_offset: u64,
    pub lma_offset: u64,
}

pub struct GraphResources<'data, 'scope, P: Platform> {
    pub symbol_db: &'scope SymbolDb<'data, P>,

    pub output_sections: &'scope OutputSections<'data, P>,

    pub worker_slots: Vec<Mutex<WorkerSlot<'data, P>>>,

    pub errors: Mutex<Vec<Error>>,

    pub per_symbol_flags: &'scope AtomicPerSymbolFlags<'scope>,

    /// Sections that we'll keep, even if their total size is zero.
    pub must_keep_sections: OutputSectionMap<AtomicBool>,

    pub has_static_tls: AtomicBool,

    pub has_variant_pcs: AtomicBool,

    pub thunk_layout_builder: Option<crate::thunks::ThunkLayoutBuilder>,

    pub layout_resources_ext: P::LayoutResourcesExt<'data>,
}

pub struct FinaliseLayoutResources<'scope, 'data, P: Platform> {
    pub symbol_db: &'scope SymbolDb<'data, P>,
    pub per_symbol_flags: &'scope PerSymbolFlags,
    pub output_sections: &'scope OutputSections<'data, P>,
    pub output_order: &'scope OutputOrder<'data>,
    pub section_layouts: &'scope OutputSectionMap<OutputRecordLayout>,
    pub merged_string_start_addresses: &'scope MergedStringStartAddresses,
    pub merged_strings: &'scope OutputSectionMap<MergedStringsSection<'data>>,
    pub dynamic_symbol_definitions: &'scope Vec<DynamicSymbolDefinition<'data, P>>,
    pub segment_layouts: &'scope SegmentLayouts,
    pub program_segments: &'scope ProgramSegments<P::ProgramSegmentDef>,
    pub script_sorted_sections: &'scope [InputSortedSection],
    pub format_specific: &'scope P::FinaliseSizesExt<'data>,

    pub thunk_blocks: &'scope [crate::thunks::ThunkBlock],

    /// Per-thunk-block addresses-maps. We could store this on ObjectLayoutState, but only a small
    /// fraction of the input objects will be thunk-block owners, so it'd seem wasteful. Instead we
    /// put it here and wrap each map in a mutex. Since each map is only written by its owner, each
    /// mutex should only ever get locked once during its lifetime.
    pub thunk_block_addresses: &'scope Vec<Mutex<BTreeMap<SymbolId, u64>>>,
}

#[derive(Copy, Clone, Debug)]
pub enum WorkItem<P: Platform> {
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
pub struct GcLoadRequest<P: Platform> {
    pub file_id: FileId,

    pub gc_unit: P::GcUnit,
}

impl<P: EnginePlatform> WorkItem<P> {
    pub fn file_id(self, symbol_db: &SymbolDb<P>) -> FileId {
        match self {
            WorkItem::LoadGlobalSymbol(s) | WorkItem::CopyRelocateSymbol(s) => {
                symbol_db.file_id_for_symbol(s)
            }
            WorkItem::LoadGcUnit(s) => s.file_id,
            WorkItem::ExportDynamic(symbol_id) => symbol_db.file_id_for_symbol(symbol_id),
        }
    }
}

#[derive(Clone)]
pub struct MemoryRegion {
    pub origin: u64,
    pub length: u64,
    pub used: u64,
    pub used_lma: u64,
    pub flags: Option<wild_scripts::linker_script::MemoryFlags>,
}

impl<'data, P: EnginePlatform> Layout<'data, P> {
    pub fn prelude(&self) -> &PreludeLayout<'data, P> {
        let Some(FileLayout::Prelude(i)) = self.group_layouts.first().and_then(|g| g.files.first())
        else {
            panic!("Prelude layout not found at expected offset");
        };
        i
    }

    pub fn args(&self) -> &'data P::Args {
        self.symbol_db.args
    }

    /// Symbol-bearing inputs for incremental atom binding and skip planning.
    pub fn incremental_file_records(&self) -> Vec<crate::incremental::IncrementalFileRecord> {
        let mut records = Vec::new();
        for group in &self.group_layouts {
            for file in &group.files {
                match file {
                    FileLayout::Prelude(prelude) => {
                        records.push(crate::incremental::IncrementalFileRecord {
                            file_id: wild_platform::PRELUDE_FILE_ID,
                            key: "<prelude>".into(),
                            source_path: PathBuf::new(),
                            sizes: Vec::new(),
                            num_symbols: prelude.internal_symbols.symbol_definitions.len(),
                            skippable: false,
                        });
                    }
                    FileLayout::Object(obj) => {
                        let sizes = obj
                            .sections
                            .iter()
                            .filter_map(|slot| match slot {
                                SectionSlot::Loaded(sec) => Some(sec.size),
                                _ => None,
                            })
                            .collect();
                        records.push(crate::incremental::IncrementalFileRecord {
                            file_id: obj.file_id,
                            key: obj.input.to_string(),
                            source_path: obj.input.file.filename.to_path_buf(),
                            sizes,
                            num_symbols: obj.symbol_id_range.len(),
                            skippable: true,
                        });
                    }
                    FileLayout::Dynamic(dyn_obj) => {
                        records.push(crate::incremental::IncrementalFileRecord {
                            file_id: dyn_obj.file_id,
                            key: dyn_obj.input.to_string(),
                            source_path: dyn_obj.input.file.filename.to_path_buf(),
                            sizes: Vec::new(),
                            num_symbols: dyn_obj.symbol_id_range.len(),
                            skippable: false,
                        });
                    }
                    FileLayout::LinkerScript(script) => {
                        records.push(crate::incremental::IncrementalFileRecord {
                            file_id: script.file_id,
                            key: script.input.to_string(),
                            source_path: script.input.file.filename.to_path_buf(),
                            sizes: Vec::new(),
                            num_symbols: script.symbol_id_range.len(),
                            skippable: false,
                        });
                    }
                    FileLayout::SyntheticSymbols(syn) => {
                        if syn.internal_symbols.symbol_definitions.is_empty() {
                            continue;
                        }
                        records.push(crate::incremental::IncrementalFileRecord {
                            file_id: self
                                .symbol_db
                                .file_id_for_symbol(syn.internal_symbols.start_symbol_id),
                            key: "<synthetic>".into(),
                            source_path: PathBuf::new(),
                            sizes: Vec::new(),
                            num_symbols: syn.internal_symbols.symbol_definitions.len(),
                            skippable: false,
                        });
                    }
                    FileLayout::Epilogue(_)
                    | FileLayout::StubLibrary(_)
                    | FileLayout::NotLoaded => {}
                }
            }
        }
        records
    }

    pub fn incremental_resolutions(&self) -> crate::incremental::AtomResolutions {
        let raw: Vec<u64> = self.symbol_resolutions.raw_values().collect();
        let mut out = crate::incremental::AtomResolutions::default();
        for (file_id, atom) in &self.incremental_atoms {
            let range = self.symbol_db.file(*file_id).symbol_id_range();
            let mut values = Vec::with_capacity(range.len());
            for id in range {
                values.push(raw.get(id.as_usize()).copied().unwrap_or(0));
            }
            out.set(*atom, values);
        }
        out
    }

    pub fn skip_incremental_payload(&self, file_id: FileId) -> bool {
        self.incremental_skip_payloads.contains(&file_id)
    }

    pub fn record_reverse_reloc(
        &self,
        symbol_id: SymbolId,
        file_offset: u64,
        place: u64,
        addend: i64,
        r_type: u32,
        file_id: FileId,
    ) {
        if !self.args().incremental() {
            return;
        }
        let Some(&owner) = self.incremental_atoms.get(&file_id) else {
            return;
        };
        let defined = self.symbol_db.definition(symbol_id);
        let def_file = self.symbol_db.file_id_for_symbol(defined);
        let Some(&def_atom) = self.incremental_atoms.get(&def_file) else {
            return;
        };
        let local = self
            .symbol_db
            .file(def_file)
            .symbol_id_range()
            .id_to_offset(defined);
        self.incremental_reverse_relocs.lock().unwrap().push(
            def_atom,
            local,
            file_offset,
            place,
            addend,
            r_type,
            owner,
        );
    }

    pub fn take_reverse_relocs(&self) -> crate::incremental::ReverseRelocIndex {
        replace(
            &mut *self.incremental_reverse_relocs.lock().unwrap(),
            crate::incremental::ReverseRelocIndex::new(),
        )
    }

    pub fn symbol_debug<'layout>(
        &'layout self,
        symbol_id: SymbolId,
    ) -> SymbolDebug<'layout, 'data, P> {
        self.symbol_db
            .symbol_debug(&self.per_symbol_flags, symbol_id)
    }

    #[inline(always)]
    pub fn merged_symbol_resolution(&self, symbol_id: SymbolId) -> Option<Resolution<P>> {
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

    pub fn local_symbol_resolution(&self, symbol_id: SymbolId) -> Option<&Resolution<P>> {
        self.symbol_resolutions.get(symbol_id)
    }

    pub fn resolutions_in_range(
        &self,
        range: SymbolIdRange,
    ) -> impl Iterator<Item = (SymbolId, Option<&Resolution<P>>)> {
        self.symbol_resolutions.resolutions[range.as_usize()]
            .iter()
            .enumerate()
            .map(move |(i, res)| (range.offset_to_id(i), res.as_ref()))
    }

    pub fn resolved_entry_symbol_address(&self) -> Result<Option<u64>> {
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

    pub fn tls_start_address(&self) -> u64 {
        // If we don't have a TLS segment then the value we return won't really matter.
        self.segment_layouts
            .tls_layout
            .as_ref()
            .map_or(0, |seg| seg.mem_offset)
    }

    pub fn tls_start_address_aligned(&self) -> u64 {
        self.segment_layouts
            .tls_layout
            .as_ref()
            .map_or(0, |seg| seg.alignment.align_down(seg.mem_offset))
    }

    /// Returns the memory address of the end of the TLS segment including any padding required to
    /// make sure that the TCB will be usize-aligned.
    pub fn tls_end_address(&self) -> u64 {
        self.segment_layouts.tls_layout.as_ref().map_or(0, |seg| {
            seg.alignment.align_up(seg.mem_offset + seg.mem_size)
        })
    }

    /// Returns the memory address of the start of the TLS segment used by the AArch64.
    pub fn tls_start_address_aarch64(&self) -> u64 {
        self.segment_layouts.tls_layout.as_ref().map_or(0, |seg| {
            seg.alignment
                .align_down(seg.mem_offset - linker_utils::aarch64::TLS_TCB_SIZE)
        })
    }

    pub fn tlv_data_start_address(&self) -> u64 {
        self.output_sections
            .ids_with_info()
            .filter(|(_, info)| info.section_attributes.is_tls())
            .map(|(id, _)| self.section_layouts.get(id).mem_offset)
            .min()
            .unwrap_or(0)
    }

    pub fn layout_data(&self) -> linker_layout::Layout {
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

    pub fn thunk_count(&self) -> u64 {
        self.thunk_block_addresses
            .iter()
            .map(|m| m.len() as u64)
            .sum()
    }

    pub fn flags_for_symbol(&self, symbol_id: SymbolId) -> ValueFlags {
        self.symbol_db
            .flags_for_symbol(&self.per_symbol_flags, symbol_id)
    }

    pub fn file_layout(&self, file_id: FileId) -> &FileLayout<'data, P> {
        let group_layout = &self.group_layouts[file_id.group()];
        &group_layout.files[file_id.file()]
    }

    /// Returns the base address of the global offset table. This needs to be consistent with the
    /// symbol `_GLOBAL_OFFSET_TABLE_`.
    pub fn got_base(&self) -> u64 {
        let got_layout = self
            .section_layouts
            .get(P::GOT_SECTION_ID.expect("platform has no GOT section"));
        got_layout.mem_offset
    }

    /// Returns whether we're going to output the .gnu.version section.
    pub fn gnu_version_enabled(&self) -> bool {
        P::GNU_VERSION_SECTION_ID.is_some_and(|section_id| {
            self.section_part_layouts
                .get(section_id.base_part_id::<P>())
                .file_size
                > 0
        })
    }
}

#[derive(Default)]
pub struct WorkerSlot<'data, P: Platform> {
    pub work: Vec<WorkItem<P>>,
    pub worker: Option<GroupState<'data, P>>,
}

#[derive(Debug)]
pub struct GcOutputs<'data, P: Platform> {
    pub group_states: Vec<GroupState<'data, P>>,
    pub must_keep_sections: OutputSectionMap<bool>,
    pub has_static_tls: bool,
    pub has_variant_pcs: bool,
    pub thunk_layout_builder: Option<ThunkLayoutBuilder>,
}

pub struct GroupActivationInputs<'data, P: Platform> {
    pub resolved: ResolvedGroup<'data, P>,
    pub num_symbols: usize,
    pub group_index: usize,
}

#[derive(Debug)]
pub struct HeaderInfo {
    pub num_output_sections_with_content: u32,
    pub partial_link_section_name_bytes: u64,
    pub active_segment_ids: Vec<ProgramSegmentId>,
}

pub struct ResolutionWriter<'writer, 'out, P: Platform> {
    pub resolutions_out: &'writer mut sharded_vec_writer::Shard<'out, Option<Resolution<P>>>,
}

impl<P: EnginePlatform> ResolutionWriter<'_, '_, P> {
    pub fn write(&mut self, res: Option<Resolution<P>>) -> Result {
        self.resolutions_out.try_push(res)?;
        Ok(())
    }
}

impl<'data, P: EnginePlatform> resolution::ResolvedFile<'data, P> {
    pub fn create_layout_state(self, args: &P::Args) -> FileLayoutState<'data, P> {
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

impl<P: EnginePlatform> Resolution<P> {
    pub fn flags(self) -> ValueFlags {
        self.flags
    }

    pub fn value(self) -> u64 {
        self.raw_value
    }

    pub fn address(&self) -> Result<u64> {
        if !self.flags.has_link_time_address() {
            bail!("Expected address, found {}", self.flags);
        }
        Ok(self.raw_value)
    }

    pub fn value_for_symbol_table(&self) -> u64 {
        self.raw_value
    }

    pub fn is_absolute(&self) -> bool {
        self.flags.is_absolute()
    }

    pub fn dynamic_symbol_index(&self) -> Result<u32> {
        Ok(self
            .dynamic_symbol_index
            .context("Missing dynamic_symbol_index")?
            .get())
    }
}

/// Maximum number of relaxation scan iterations. In practice convergence
/// happens in 2–3 passes.
pub const MAX_RELAXATION_ITERATIONS: usize = 5;

/// Sentinel value stored in `SymbolOutputInfos::addresses` for symbols whose output address is
/// unknown.
pub const SYMBOL_ADDRESS_UNRESOLVED: u64 = u64::MAX;

#[derive(Debug, Clone, Copy)]
pub struct InputSectionPosition {
    pub part_id: PartId,
    pub address: u64,
}

/// Input-section positions in the coordinate system supplied by the initial part offsets.
/// Zero initial offsets produce part-relative positions, while final part offsets produce output
/// addresses.
pub type InputSectionPositions = Vec<Vec<Vec<Option<InputSectionPosition>>>>;

/// Stores precomputed output-address information for every symbol.
pub struct SymbolOutputInfos {
    pub addresses: Vec<u64>,
}

impl SymbolOutputInfos {
    pub fn resolve(
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

/// Per-file list of section indices to rescan on subsequent relaxation iterations. Indexed as
/// `[group_idx][file_idx]`.  Files that are not objects get an empty entry.
pub type RescanSections = Vec<Vec<SmallVec<[usize; 16]>>>;

/// Like `RescanSections` but each entry also carries the minimum margin (in bytes) among the
/// section's unrelaxed candidates.  This is returned by `relaxation_scan_pass` and then filtered
/// by `total_deleted` to produce a `RescanSections` for the next iteration.
pub type RescanCandidates = Vec<Vec<SmallVec<[(usize, u64); 16]>>>;

pub struct InputOrderItem {
    pub part_id: PartId,
    pub group_idx: usize,
    pub link_order: u32,
    pub alignment: Alignment,
    pub size: u64,
}

pub fn object_symbol_address_in_layout<'data, P: EnginePlatform>(
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
pub fn compute_segment_alignments<'data, P: EnginePlatform>(
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
                let max_alignment = crate::output_section_part_map::max_alignment(
                    sizes,
                    part_id_range,
                    output_sections,
                );

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

impl<'data, P: EnginePlatform> Layout<'data, P> {
    pub fn mem_address_of_built_in(&self, section_id: OutputSectionId) -> u64 {
        self.section_layouts.get(section_id).mem_offset
    }
}

impl<'scope, 'data, P: EnginePlatform> FinaliseLayoutResources<'scope, 'data, P> {
    pub fn symbol_debug<'a>(&'a self, symbol_id: SymbolId) -> SymbolDebug<'a, 'data, P> {
        self.symbol_db
            .symbol_debug(self.per_symbol_flags, symbol_id)
    }
}

impl OutputRecordLayout {
    pub fn file_end(&self) -> usize {
        self.file_offset + self.file_size
    }

    pub fn mem_end(&self) -> u64 {
        self.mem_offset + self.mem_size
    }

    pub fn merge(&mut self, other: &OutputRecordLayout) {
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
