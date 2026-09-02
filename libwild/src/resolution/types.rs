use crate::error::Error;
use crate::grouping::SequencedInputObject;
use crate::input_data::FileId;
use crate::input_data::InputRef;
use crate::input_section_id::SectionIdRange;
use crate::macho_stub_library::DefinedStubLibrary;
use crate::output_section_id::CustomSectionDetails;
use crate::output_section_id::InitFiniSectionDetail;
use crate::parsing::InternalSymDefInfo;
use crate::platform::DynamicTagValues as _;
use crate::platform::FrameIndex;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::string_merging::StringMergeSectionExtra;
use crate::string_merging::StringMergeSectionSlot;
use crate::symbol::PreHashedSymbolName;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolIdRange;
use crate::symbol_db::SymbolStrength;
use crossbeam_queue::ArrayQueue;
use crossbeam_queue::SegQueue;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

pub(super) const MAX_SYMBOLS_PER_WORK_ITEM: usize = 5000;

/// A request to load a chunk of symbols from an object.
pub(super) struct LoadObjectSymbolsRequest<'definitions> {
    /// The ID of the object to load.
    pub(super) file_id: FileId,

    pub(super) symbol_start_offset: usize,

    /// The symbol resolutions for the object to be loaded that should be written to when we load
    /// the object.
    pub(super) definitions_out: &'definitions mut [SymbolId],
}

#[derive(Default)]
pub(crate) struct LoadedMetrics {
    pub(crate) loaded_bytes: AtomicUsize,
    pub(crate) loaded_compressed_bytes: AtomicUsize,
    pub(crate) decompressed_bytes: AtomicUsize,
}

impl LoadedMetrics {
    pub(super) fn log(&self) {
        let loaded_bytes = self.loaded_bytes.load(Ordering::Relaxed);
        let loaded_compressed_bytes = self.loaded_compressed_bytes.load(Ordering::Relaxed);
        let decompressed_bytes = self.decompressed_bytes.load(Ordering::Relaxed);
        tracing::debug!(target: "metrics", loaded_bytes, loaded_compressed_bytes, decompressed_bytes, "input_sections");
    }
}
#[derive(Debug)]
pub(crate) struct ResolvedGroup<'data, P: Platform> {
    pub(crate) files: Vec<ResolvedFile<'data, P>>,
}

#[derive(Debug)]
pub(crate) enum ResolvedFile<'data, P: Platform> {
    NotLoaded(NotLoaded),
    Prelude(ResolvedPrelude<'data, P>),
    Object(ResolvedObject<'data, P>),
    Dynamic(ResolvedDynamic<'data, P>),
    StubLibrary(ResolvedStubLibrary<'data>),
    LinkerScript(ResolvedLinkerScript<'data, P>),
    SyntheticSymbols(ResolvedSyntheticSymbols<'data, P>),
    #[cfg(all(feature = "plugins", unix))]
    LtoInput(ResolvedLtoInput),
}

#[derive(Debug)]
pub(crate) struct NotLoaded {
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) section_id_range: SectionIdRange,
}

/// A section, but where we may or may not yet have decided to load it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SectionSlot {
    /// We've decided that this section won't be loaded.
    Discard,

    /// The section hasn't been loaded yet, but may be loaded if it's referenced.
    Unloaded(UnloadedSection),

    /// The section had the retain bit set, so must be loaded.
    MustLoad(UnloadedSection),

    /// We've already loaded the section.
    Loaded(crate::layout::Section),

    /// As for `Loaded`, but responsibility for allocating and writing the section is held by the
    /// epilogue due to being part of a sorted section.
    Sorted(crate::layout::SortedSection),

    /// The section contains frame data, e.g. .eh_frame or equivalent.
    FrameData(object::SectionIndex),

    /// The section is a string-merge section.
    MergeStrings(StringMergeSectionSlot),

    // The section contains a debug info section that might be loaded.
    UnloadedDebugInfo,

    // Loaded section with debug info content.
    LoadedDebugInfo(crate::layout::Section),

    // GNU property section (.note.gnu.property)
    NoteGnuProperty(object::SectionIndex),

    // RISC-V attributes section (.riscv.attributes)
    RiscvVAttributes(object::SectionIndex),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UnloadedSection {
    /// The index of the last FDE for this section. Previous FDEs will be linked from this.
    pub(crate) last_frame_index: Option<FrameIndex>,

    /// Whether the section has a name that makes it eligible for generation of __start_ / __stop_
    /// symbols. In particular, the name of the section doesn't start with a ".".
    pub(crate) start_stop_eligible: bool,

    pub(crate) needs_sorting: bool,
    pub(crate) sort_by_init_priority: bool,
}

impl UnloadedSection {
    pub(super) fn new() -> Self {
        Self {
            last_frame_index: None,
            start_stop_eligible: false,
            needs_sorting: false,
            sort_by_init_priority: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedPrelude<'data, P: Platform> {
    pub(crate) symbol_definitions: Vec<InternalSymDefInfo<'data, P>>,
}

/// Resolved state common to dynamic and regular objects.
#[derive(Debug)]
pub(crate) struct ResolvedCommon<'data, P: Platform> {
    pub(crate) input: InputRef<'data>,
    pub(crate) object: &'data P::File<'data>,
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) link_order: u32,
}
#[derive(Debug, Clone)]
pub(crate) struct ScriptSortedSectionDetail {
    pub(crate) index: object::SectionIndex,
    pub(crate) sort_by_init_priority: bool,
}

#[derive(Debug)]
pub(crate) struct ResolvedObject<'data, P: Platform> {
    pub(crate) common: ResolvedCommon<'data, P>,
    pub(crate) section_id_range: SectionIdRange,

    pub(crate) sections: Vec<SectionSlot>,
    pub(crate) relocations: P::RelocationSections,

    pub(crate) string_merge_extras: Vec<StringMergeSectionExtra<'data>>,

    /// Details about each custom section that is defined in this object.
    pub(super) custom_sections: Vec<CustomSectionDetails<'data, P>>,

    pub(super) init_fini_sections: Vec<InitFiniSectionDetail>,

    pub(crate) script_sorted_sections: Vec<ScriptSortedSectionDetail>,

    /// Total size in bytes of all executable input sections in this object. Used to determine
    /// early-on if we can be sure that thunks won't be needed.
    pub(crate) executable_bytes: u64,

    pub(crate) format_specific: P::ResolvedObjectExt<'data>,
}

#[derive(Debug)]
pub(crate) struct ResolvedDynamic<'data, P: Platform> {
    pub(crate) common: ResolvedCommon<'data, P>,
    dynamic_tag_values: P::DynamicTagValues<'data>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedStubLibrary<'data> {
    pub(crate) input: InputRef<'data>,
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) defined_symbols: DefinedStubLibrary<'data>,
}

#[derive(Debug)]
pub(crate) struct ResolvedLinkerScript<'data, P: Platform> {
    pub(crate) input: InputRef<'data>,
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) symbol_definitions: Vec<InternalSymDefInfo<'data, P>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedSyntheticSymbols<'data, P: Platform> {
    pub(crate) file_id: FileId,
    pub(crate) start_symbol_id: SymbolId,
    pub(crate) symbol_definitions: Vec<InternalSymDefInfo<'data, P>>,
}

#[cfg(all(feature = "plugins", unix))]
#[derive(Debug, Clone)]
pub(crate) struct ResolvedLtoInput {
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) section_id_range: SectionIdRange,
}
pub(super) struct Outputs<'data, P: Platform> {
    /// Where we put objects once we've loaded them.
    pub(super) loaded: ArrayQueue<ResolvedFile<'data, P>>,

    #[cfg(all(feature = "plugins", unix))]
    pub(super) loaded_lto_objects: ArrayQueue<ResolvedLtoInput>,

    /// Any errors that we encountered.
    pub(super) errors: ArrayQueue<Error>,

    pub(super) undefined_symbols: SegQueue<UndefinedSymbol<'data>>,
}

impl<'data, P: Platform> Outputs<'data, P> {
    #[allow(unused_variables)]
    pub(super) fn new(num_regular_objects: usize, num_lto_objects: usize) -> Self {
        Self {
            loaded: ArrayQueue::new(num_regular_objects.max(1)),
            #[cfg(all(feature = "plugins", unix))]
            loaded_lto_objects: ArrayQueue::new(num_lto_objects.max(1)),
            errors: ArrayQueue::new(1),
            undefined_symbols: SegQueue::new(),
        }
    }
}
pub(super) struct UndefinedSymbol<'data> {
    /// If we have a file ID here and that file is loaded, then the symbol is actually defined and
    /// this record can be ignored.
    pub(super) ignore_if_loaded: Option<FileId>,
    pub(super) name: PreHashedSymbolName<'data>,
    pub(super) symbol_id: SymbolId,
}
impl<'data, P: Platform> ResolvedCommon<'data, P> {
    pub(super) fn new(obj: &'data SequencedInputObject<'data, P>) -> Self {
        Self {
            input: obj.parsed.input,
            object: &obj.parsed.object,
            file_id: obj.file_id,
            symbol_id_range: obj.symbol_id_range,
            link_order: obj.link_order,
        }
    }

    pub(crate) fn symbol_strength(&self, symbol_id: SymbolId) -> SymbolStrength {
        let local_index = symbol_id.to_input(self.symbol_id_range);
        let Ok(obj_symbol) = self.object.symbol(local_index) else {
            // Errors from this function should have been reported elsewhere.
            return SymbolStrength::Undefined;
        };
        SymbolStrength::of(obj_symbol)
    }
}
impl<'data, P: Platform> ResolvedObject<'data, P> {
    pub(super) fn new(common: ResolvedCommon<'data, P>, section_id_range: SectionIdRange) -> Self {
        let format_specific = P::new_resolved_object_ext(common.symbol_id_range, common.file_id);
        Self {
            common,
            section_id_range,
            // We'll fill this the rest during section resolution.
            sections: Default::default(),
            relocations: Default::default(),
            string_merge_extras: Default::default(),
            custom_sections: Default::default(),
            init_fini_sections: Default::default(),
            script_sorted_sections: Default::default(),
            executable_bytes: 0,
            format_specific,
        }
    }
}

impl<'data, P: Platform> ResolvedDynamic<'data, P> {
    pub(super) fn new(
        common: ResolvedCommon<'data, P>,
        dynamic_tag_values: P::DynamicTagValues<'data>,
    ) -> Self {
        Self {
            common,
            dynamic_tag_values,
        }
    }

    pub(crate) fn lib_name(&self) -> &'data [u8] {
        self.dynamic_tag_values.lib_name(&self.common.input)
    }
}
#[derive(Debug)]
pub(crate) struct SymbolAttributes<'data, P: Platform> {
    pub(crate) is_local: bool,
    pub(crate) default_visibility: bool,
    pub(crate) is_weak: bool,
    pub(crate) name_info: P::RawSymbolName<'data>,
}
impl<'data, P: Platform> std::fmt::Display for ResolvedObject<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.common.input, f)
    }
}

impl<'data, P: Platform> std::fmt::Display for ResolvedDynamic<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.common.input, f)
    }
}

impl ResolvedStubLibrary<'_> {
    pub(crate) fn symbol_strength(&self, symbol_id: SymbolId) -> SymbolStrength {
        let local_index = self.symbol_id_range.id_to_offset(symbol_id);
        if local_index < self.defined_symbols.symbols.len() {
            SymbolStrength::Strong
        } else {
            SymbolStrength::Weak
        }
    }
}

impl std::fmt::Display for ResolvedStubLibrary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)
    }
}

impl<'data, P: Platform> std::fmt::Display for ResolvedLinkerScript<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)
    }
}

impl<'data, P: Platform> std::fmt::Display for ResolvedFile<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolvedFile::NotLoaded(_) => std::fmt::Display::fmt("<not loaded>", f),
            ResolvedFile::Prelude(_) => std::fmt::Display::fmt("<prelude>", f),
            ResolvedFile::Object(o) => std::fmt::Display::fmt(o, f),
            ResolvedFile::Dynamic(o) => std::fmt::Display::fmt(o, f),
            ResolvedFile::StubLibrary(o) => std::fmt::Display::fmt(o, f),
            ResolvedFile::LinkerScript(o) => std::fmt::Display::fmt(o, f),
            ResolvedFile::SyntheticSymbols(_) => std::fmt::Display::fmt("<synthetic>", f),
            #[cfg(all(feature = "plugins", unix))]
            ResolvedFile::LtoInput(_) => std::fmt::Display::fmt("<lto object>", f),
        }
    }
}

impl SectionSlot {
    pub(crate) fn is_loaded(&self) -> bool {
        !matches!(
            self,
            SectionSlot::Discard | SectionSlot::Unloaded(..) | SectionSlot::NoteGnuProperty(..)
        )
    }

    pub(crate) fn unloaded_mut(&mut self) -> Option<&mut UnloadedSection> {
        match self {
            SectionSlot::Unloaded(unloaded) | SectionSlot::MustLoad(unloaded) => Some(unloaded),
            _ => None,
        }
    }
}

impl<'data, P: Platform> ResolvedFile<'data, P> {
    pub(super) fn symbol_id_range(&self) -> SymbolIdRange {
        match self {
            ResolvedFile::NotLoaded(s) => s.symbol_id_range,
            ResolvedFile::Prelude(s) => s.symbol_id_range(),
            ResolvedFile::Object(s) => s.common.symbol_id_range,
            ResolvedFile::Dynamic(s) => s.common.symbol_id_range,
            ResolvedFile::StubLibrary(s) => s.symbol_id_range,
            ResolvedFile::LinkerScript(s) => s.symbol_id_range,
            ResolvedFile::SyntheticSymbols(s) => s.symbol_id_range(),
            #[cfg(all(feature = "plugins", unix))]
            ResolvedFile::LtoInput(s) => s.symbol_id_range,
        }
    }
}

impl<'data, P: Platform> ResolvedPrelude<'data, P> {
    pub(super) fn symbol_id_range(&self) -> SymbolIdRange {
        SymbolIdRange::input(SymbolId::undefined(), self.symbol_definitions.len())
    }
}

impl<'data, P: Platform> ResolvedSyntheticSymbols<'data, P> {
    pub(super) fn symbol_id_range(&self) -> SymbolIdRange {
        SymbolIdRange::input(self.start_symbol_id, self.symbol_definitions.len())
    }
}
// We create quite a lot of `SectionSlot`s. We don't generally copy them, however we do need to
// eventually drop the Vecs that contain them. Dropping those Vecs is a lot cheaper if the slots
// don't need to have run Drop. We check for this, by making sure the type implements `Copy`
#[test]
fn section_slot_is_copy() {
    fn assert_copy<T: Copy>(_v: T) {}

    assert_copy(SectionSlot::Discard);
}
