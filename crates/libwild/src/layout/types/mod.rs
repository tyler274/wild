mod gc;
mod objects;
mod units;

use super::graph::*;
use crate::alignment::Alignment;
use crate::bail;
use crate::compression::CompressedSection;
use crate::error::Context;
use crate::error::Error;
use crate::error::Result;
use crate::expression_eval::ResolvedLocationCounter;
use crate::grouping::SequencedInputObject;
use crate::input_data::FileId;
use crate::input_data::InputRef;
use crate::input_section_id::SectionIdRange;
use crate::layout::EnginePlatform;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::InternalSymDefInfo;
use crate::part_id::PartId;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::RelaxSymbolInfo;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionFlags as _;
use crate::platform::Symbol as _;
use crate::program_segments::ProgramSegmentId;
use crate::program_segments::ProgramSegments;
use crate::resolution;
use crate::resolution::NotLoaded;
use crate::resolution::ResolvedGroup;
use crate::resolution::ScriptSortedSectionDetail;
use crate::resolution::SectionSlot;
use crate::string_merging::MergedStringStartAddresses;
use crate::string_merging::MergedStringsSection;
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
#[allow(unused_imports)]
pub(crate) use gc::*;
use hashbrown::HashMap;
use hashbrown::HashSet;
use linker_utils::relaxation::RelaxDeltaMap;
#[allow(unused_imports)]
pub(crate) use objects::*;
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::mem::replace;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
#[allow(unused_imports)]
pub(crate) use units::*;

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
    /// This-run `FileId` → generational atom. Empty unless `--incremental`.
    pub(crate) incremental_atoms: HashMap<FileId, crate::incremental::AtomId>,
    /// Sites that applied a relocation, keyed by defined atom + local symbol. Empty when not
    /// incremental.
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

impl<P: EnginePlatform> SymbolResolutions<P> {
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
    pub(crate) fn full_resolution<P: EnginePlatform>(self) -> Option<Resolution<P>> {
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
    pub(crate) start_stop_sections:
        Option<OutputSectionMap<Vec<resolution::StartStopCandidate<P>>>>,
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

pub(crate) fn section_group_order<P: EnginePlatform>(
    files: &[FileLayoutState<P>],
) -> SectionGroupOrder {
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

impl<P: EnginePlatform> WorkItem<P> {
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

impl<'data, P: EnginePlatform> Layout<'data, P> {
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

    /// Symbol-bearing inputs for incremental atom binding and skip planning.
    pub(crate) fn incremental_file_records(
        &self,
    ) -> Vec<crate::incremental::IncrementalFileRecord> {
        let mut records = Vec::new();
        for group in &self.group_layouts {
            for file in &group.files {
                match file {
                    FileLayout::Prelude(prelude) => {
                        records.push(crate::incremental::IncrementalFileRecord {
                            file_id: crate::input_data::PRELUDE_FILE_ID,
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

    pub(crate) fn incremental_resolutions(&self) -> crate::incremental::AtomResolutions {
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

    pub(crate) fn take_reverse_relocs(&self) -> crate::incremental::ReverseRelocIndex {
        replace(
            &mut *self.incremental_reverse_relocs.lock().unwrap(),
            crate::incremental::ReverseRelocIndex::new(),
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

    pub(crate) fn tlv_data_start_address(&self) -> u64 {
        self.output_sections
            .ids_with_info()
            .filter(|(_, info)| info.section_attributes.is_tls())
            .map(|(id, _)| self.section_layouts.get(id).mem_offset)
            .min()
            .unwrap_or(0)
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

#[derive(Debug)]
pub(crate) struct HeaderInfo {
    pub(crate) num_output_sections_with_content: u32,
    pub(crate) active_segment_ids: Vec<ProgramSegmentId>,
}

pub(crate) struct ResolutionWriter<'writer, 'out, P: Platform> {
    pub(crate) resolutions_out: &'writer mut sharded_vec_writer::Shard<'out, Option<Resolution<P>>>,
}

impl<P: EnginePlatform> ResolutionWriter<'_, '_, P> {
    pub(crate) fn write(&mut self, res: Option<Resolution<P>>) -> Result {
        self.resolutions_out.try_push(res)?;
        Ok(())
    }
}

impl<'data, P: EnginePlatform> resolution::ResolvedFile<'data, P> {
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

impl<P: EnginePlatform> Resolution<P> {
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

pub(crate) fn object_symbol_address_in_layout<'data, P: EnginePlatform>(
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
pub(crate) fn compute_segment_alignments<'data, P: EnginePlatform>(
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

impl<'data, P: EnginePlatform> Layout<'data, P> {
    pub(crate) fn mem_address_of_built_in(&self, section_id: OutputSectionId) -> u64 {
        self.section_layouts.get(section_id).mem_offset
    }
}

impl<'scope, 'data, P: EnginePlatform> FinaliseLayoutResources<'scope, 'data, P> {
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
