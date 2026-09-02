use super::SinglePartSectionId;
use super::WASM_MAGIC;
use super::Wasm;
use super::file::*;
use super::gc::*;
use super::linking::*;
use super::output::*;
use super::relocations::*;
use super::symbols::*;
use crate::FileSystem;
use crate::args::wasm::WasmArgs;
use crate::error::Result;
use crate::layout;
use crate::layout_rules::SectionKind;
use crate::layout_rules::SectionRule;
use crate::layout_rules::SectionRuleOutcome;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionIdentity;
use crate::output_section_id::SectionName;
use crate::platform;
use crate::platform::Args as _;
use wasmparser::RelocationType;
use wasmparser::SymbolFlags;

impl platform::SectionHeader for SectionHeader {
    fn is_alloc(&self) -> bool {
        true
    }

    fn is_writable(&self) -> bool {
        // Wasm sections are not classified into RW vs RO at the section level.
        false
    }

    fn is_executable(&self) -> bool {
        // Code lives in the dedicated CODE section.
        false
    }

    fn is_tls(&self) -> bool {
        // Wasm has no TLS yet.
        false
    }

    fn is_merge_section(&self) -> bool {
        false
    }

    fn is_strings(&self) -> bool {
        false
    }

    fn should_retain(&self) -> bool {
        false
    }

    fn should_exclude(&self) -> bool {
        false
    }

    fn is_group(&self) -> bool {
        false
    }

    fn is_note(&self) -> bool {
        false
    }

    fn is_prog_bits(&self) -> bool {
        true
    }

    fn is_no_bits(&self) -> bool {
        false
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct SectionType {}

impl platform::SectionType for SectionType {
    fn is_rela(&self) -> bool {
        false
    }

    fn is_rel(&self) -> bool {
        false
    }

    fn is_symtab(&self) -> bool {
        false
    }

    fn is_strtab(&self) -> bool {
        false
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct SectionFlags {}

impl platform::SectionFlags for SectionFlags {
    fn is_alloc(self) -> bool {
        // All Wasm sections are conceptually loaded.
        true
    }
}

impl platform::Symbol for WasmSymbol {
    fn as_common(&self) -> Option<platform::CommonSymbol> {
        // Wasm has no COMMON symbols.
        None
    }

    fn is_undefined(&self) -> bool {
        WasmSymbol::is_undefined(self)
    }

    fn is_local(&self) -> bool {
        WasmSymbol::is_local(self)
    }

    fn is_absolute(&self) -> bool {
        self.raw_flags().contains(SymbolFlags::ABSOLUTE)
    }

    fn is_weak(&self) -> bool {
        WasmSymbol::is_weak(self)
    }

    fn visibility(&self) -> crate::symbol_db::Visibility {
        if self.is_hidden() {
            crate::symbol_db::Visibility::Hidden
        } else {
            crate::symbol_db::Visibility::Default
        }
    }

    fn value(&self) -> u64 {
        match self.kind {
            WasmSymbolKind::Data => u64::from(self.offset),
            _ => u64::from(self.index),
        }
    }

    fn size(&self) -> u64 {
        u64::from(self.size)
    }

    fn has_name(&self) -> bool {
        WasmSymbol::has_name(self)
    }

    fn is_default_strippable(&self, _name: &[u8]) -> bool {
        // No equivalent of ELF's `.L` local symbol convention.
        false
    }

    fn debug_string(&self) -> String {
        format!("<Wasm symbol kind={:?} index={}>", self.kind, self.index)
    }

    fn is_tls(&self) -> bool {
        self.raw_flags().contains(SymbolFlags::TLS)
    }

    fn is_interposable(&self) -> bool {
        // No dynamic linking yet; symbols can't be interposed at runtime.
        false
    }

    fn is_func(&self) -> bool {
        self.kind == WasmSymbolKind::Func
    }

    fn is_ifunc(&self) -> bool {
        false
    }

    fn is_hidden(&self) -> bool {
        WasmSymbol::is_hidden(self)
    }

    fn is_gnu_unique(&self) -> bool {
        false
    }

    fn with_hidden(mut self, hidden: bool) -> Self {
        let bit = SymbolFlags::VISIBILITY_HIDDEN.bits();
        if hidden {
            self.flags |= bit;
        } else {
            self.flags &= !bit;
        }
        self
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct SectionAttributes {}

impl platform::SectionAttributes for SectionAttributes {
    type Platform = Wasm;

    fn merge(&mut self, _rhs: Self) {
        // No per-section attributes to merge yet.
    }

    fn apply(
        &self,
        _output_sections: &mut crate::output_section_id::OutputSections<Self::Platform>,
        _section_id: crate::output_section_id::OutputSectionId,
    ) {
        // No-op: Wasm output sections inherit their attributes from `SECTION_DEFINITIONS`.
    }

    fn is_null(&self) -> bool {
        false
    }

    fn is_alloc(&self) -> bool {
        true
    }

    fn is_executable(&self) -> bool {
        false
    }

    fn is_tls(&self) -> bool {
        false
    }

    fn occupies_only_tls_address_space(&self) -> bool {
        false
    }

    fn is_writable(&self) -> bool {
        false
    }

    fn is_no_bits(&self) -> bool {
        false
    }

    fn flags(&self) -> <Self::Platform as platform::Platform>::SectionFlags {
        SectionFlags::default()
    }

    fn ty(&self) -> <Self::Platform as platform::Platform>::SectionType {
        SectionType::default()
    }

    fn set_to_default_type(&mut self) {
        // Wasm has no per-section type to reset.
    }
}

#[derive(Debug)]
pub(crate) struct NonAddressableIndexes {}

impl platform::NonAddressableIndexes for NonAddressableIndexes {
    fn new<P: platform::Platform>(_symbol_db: &crate::symbol_db::SymbolDb<P>) -> Self {
        Self {}
    }
}

/// Segment kinds used purely to drive output ordering. Wasm has no loadable program segments. These
/// variants are just a way to group the output sections in the canonical module layout.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub(crate) enum SegmentType {
    /// Holds the 8-byte module preamble.
    Header,
    /// Holds all standard Wasm sections in canonical order.
    Module,
    /// Anything not explicitly placed.
    #[default]
    Unused,
}

impl platform::SegmentType for SegmentType {}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProgramSegmentDef {
    pub(crate) segment_type: SegmentType,
}

impl std::fmt::Display for ProgramSegmentDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.segment_type)
    }
}

impl platform::ProgramSegmentDef for ProgramSegmentDef {
    fn is_writable(self) -> bool {
        false
    }

    fn is_executable(self) -> bool {
        false
    }

    fn always_keep(self) -> bool {
        true
    }

    fn is_loadable(self) -> bool {
        false
    }

    fn is_stack(self) -> bool {
        false
    }

    fn is_tls(self) -> bool {
        false
    }

    fn order_key(self) -> usize {
        self.segment_type as usize
    }
}

pub(crate) struct BuiltInSectionDetails {
    pub(crate) kind: SectionKind<'static, Wasm>,
}

impl platform::BuiltInSectionDetails for BuiltInSectionDetails {}

pub(crate) const DEFAULT_DEFS: BuiltInSectionDetails = BuiltInSectionDetails {
    kind: SectionKind::Primary(SectionIdentity::new(SectionName(&[]), ())),
};

pub(crate) const NUM_BUILT_IN_SECTIONS: usize =
    crate::output_section_id::num_built_in_sections::<Wasm>();

pub(crate) const SECTION_DEFINITIONS: [BuiltInSectionDetails; NUM_BUILT_IN_SECTIONS] = {
    use crate::layout_rules::SectionKind;
    use crate::output_section_id::SectionName;
    use crate::wasm::output_section_id as osid;

    let mut defs = [DEFAULT_DEFS; NUM_BUILT_IN_SECTIONS];

    // The module preamble.
    defs[crate::output_section_id::FILE_HEADER.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"WASM_HEADER"), ())),
    };

    // Standard Wasm sections.
    defs[osid::WASM_TYPE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"type"), ())),
    };
    defs[osid::WASM_IMPORT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"import"), ())),
    };
    defs[osid::WASM_FUNCTION.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"function"), ())),
    };
    defs[osid::WASM_TABLE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"table"), ())),
    };
    defs[osid::WASM_MEMORY.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"memory"), ())),
    };
    defs[osid::WASM_GLOBAL.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"global"), ())),
    };
    defs[osid::WASM_EXPORT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"export"), ())),
    };
    defs[osid::WASM_START.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"start"), ())),
    };
    defs[osid::WASM_ELEMENT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"element"), ())),
    };
    defs[osid::WASM_DATA_COUNT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"data_count"), ())),
    };
    defs[osid::WASM_CODE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"code"), ())),
    };
    defs[osid::WASM_DATA.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"data"), ())),
    };
    defs[osid::WASM_NAME.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"name"), ())),
    };
    defs[osid::WASM_TARGET_FEATURES.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"target_features"), ())),
    };

    defs
};

pub(crate) const PROGRAM_SEGMENT_DEFS: &[ProgramSegmentDef] = &[
    ProgramSegmentDef {
        segment_type: SegmentType::Header,
    },
    ProgramSegmentDef {
        segment_type: SegmentType::Module,
    },
    ProgramSegmentDef {
        segment_type: SegmentType::Unused,
    },
];

pub(crate) const DEFAULT_SECTION_RULES: &[SectionRule<'static>] = &[
    SectionRule::exact(b"type", SectionRuleOutcome::Discard),
    SectionRule::exact(b"import", SectionRuleOutcome::Discard),
    SectionRule::exact(b"function", SectionRuleOutcome::Discard),
    SectionRule::exact(b"table", SectionRuleOutcome::Discard),
    SectionRule::exact(b"memory", SectionRuleOutcome::Discard),
    SectionRule::exact(b"global", SectionRuleOutcome::Discard),
    SectionRule::exact(b"export", SectionRuleOutcome::Discard),
    SectionRule::exact(b"start", SectionRuleOutcome::Discard),
    SectionRule::exact(b"element", SectionRuleOutcome::Discard),
    SectionRule::exact(b"data_count", SectionRuleOutcome::Discard),
    SectionRule::exact(b"code", SectionRuleOutcome::Discard),
    SectionRule::exact(b"data", SectionRuleOutcome::Discard),
    SectionRule::exact(b"linking", SectionRuleOutcome::Discard),
    SectionRule::prefix(b"reloc.", SectionRuleOutcome::Discard),
    SectionRule::exact(b"name", SectionRuleOutcome::Discard),
    SectionRule::exact(b"target_features", SectionRuleOutcome::Discard),
];

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct DynamicTagValues<'data> {
    pub(crate) _phantom: std::marker::PhantomData<&'data [u8]>,
}

impl<'data> platform::DynamicTagValues<'data> for DynamicTagValues<'data> {
    fn lib_name(&self, input: &crate::input_data::InputRef<'data>) -> &'data [u8] {
        input.lib_name()
    }
}

#[derive(Debug)]
pub(crate) struct RawSymbolName<'data> {
    pub(crate) name: &'data [u8],
}

impl<'data> platform::RawSymbolName<'data> for RawSymbolName<'data> {
    fn parse(bytes: &'data [u8]) -> Self {
        Self { name: bytes }
    }

    fn name(&self) -> &'data [u8] {
        self.name
    }

    fn version_name(&self) -> Option<&'data [u8]> {
        None
    }

    fn is_default(&self) -> bool {
        true
    }
}

impl std::fmt::Display for RawSymbolName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&String::from_utf8_lossy(self.name), f)
    }
}

pub(crate) struct VerneedTable<'data> {
    pub(crate) _phantom: &'data [u8],
}

impl<'data> platform::VerneedTable<'data> for VerneedTable<'data> {
    fn version_name(&self, _local_symbol_index: object::SymbolIndex) -> Option<&'data [u8]> {
        None
    }
}

#[derive(Debug, Default)]
pub(crate) struct RelocationList<'data> {
    pub(crate) entries: Vec<WasmRelocation>,
    pub(crate) _phantom: std::marker::PhantomData<&'data ()>,
}

impl<'data> platform::RelocationList<'data> for RelocationList<'data> {
    fn num_relocations(&self) -> usize {
        self.entries.len()
    }
}

impl platform::Platform for Wasm {
    const NUM_SINGLE_PART_SECTIONS: u32 = SinglePartSectionId::Count as u32;
    const NUM_BUILT_IN_REGULAR_SECTIONS: usize = 0;

    const VERIFY_IGNORE_SECTION_IDS: &'static [OutputSectionId] =
        &[crate::output_section_id::FILE_HEADER];

    type File<'data> = File<'data>;
    type FileFlags = u32;
    type SymtabEntry = WasmSymbol;
    type PlatformSpecificSymbol = WasmLinkerSymbol;
    type SectionHeader = SectionHeader;
    type SectionFlags = SectionFlags;
    type SectionAttributes = SectionAttributes;
    type SectionType = SectionType;
    type SegmentType = SegmentType;
    type ProgramSegmentDef = ProgramSegmentDef;
    type BuiltInSectionDetails = BuiltInSectionDetails;
    type RelocationSections = ();
    type DynamicEntry = ();
    type DynamicSymbolDefinitionExt = ();
    type RelocationInfo = RelocationType;
    type NonAddressableIndexes = NonAddressableIndexes;
    type NonAddressableCounts = ();
    type EpilogueLayoutExt = ();
    type GroupLayoutExt = ();
    type CommonGroupStateExt = ();
    type StubLibraryLayoutStateExt = ();
    type StubLibraryLayoutExt = ();
    type ArchIdentifier = ();
    type Args = WasmArgs;
    type ResolutionExt = ();
    type SymtabShndxEntry = ();
    type SymbolVersionIndex = ();
    type FinaliseSizesExt<'data> = WasmLayout<'data>;
    type LayoutExt<'data> = WasmLayout<'data>;
    type GdbIndexScanResult<'data> = ();
    type SectionIterator<'a> = core::slice::Iter<'a, SectionHeader>;
    type DynamicTagValues<'data> = DynamicTagValues<'data>;
    type RelocationList<'data> = RelocationList<'data>;
    type DynamicLayoutStateExt<'data> = ();
    type DynamicLayoutExt<'data> = ();
    type LayoutResourcesExt<'data> = ();
    type PreludeLayoutStateExt = ();
    type PreludeLayoutExt = ();
    type ObjectLayoutStateExt<'data> = WasmObjectLayout<'data>;
    type RawSymbolName<'data> = RawSymbolName<'data>;
    type VersionNames<'data> = ();
    type VerneedTable<'data> = VerneedTable<'data>;
    type ResolvedObjectExt<'data> = WasmObjectLayout<'data>;
    type SectionIdentityExt = ();
    type GcUnit = WasmGcUnit;

    fn write_output_file<'data, A: platform::Arch<Platform = Self>, F: FileSystem>(
        output: &crate::file_writer::Output<F>,
        layout: &crate::layout::Layout<'data, Self>,
    ) -> crate::error::Result {
        output.write(layout, crate::wasm_writer::write::<A>)
    }

    fn section_attributes(_header: &Self::SectionHeader) -> Self::SectionAttributes {
        SectionAttributes::default()
    }

    fn apply_force_keep_sections(
        _keep_sections: &mut crate::output_section_map::OutputSectionMap<bool>,
        _args: &Self::Args,
    ) {
        // No `-u` / `--require-defined` analogue is wired through for Wasm yet.
    }

    fn is_zero_sized_section_content(
        _section_id: crate::output_section_id::OutputSectionId,
    ) -> bool {
        false
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
        0
    }

    fn post_gc<'data>(
        groups: &mut [crate::layout::GroupState<Self>],
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) -> crate::error::Result {
        for group in groups {
            for file in &mut group.files {
                if let crate::layout::FileLayoutState::Object(object) = file {
                    object.format_specific.compute_live_ordinals();
                }
            }
        }
        Ok(())
    }

    fn activate_dynamic<'data>(
        _state: &mut crate::layout::DynamicLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
    ) {
        // Dynamic Wasm objects are not emitted by this backend.
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
    ) -> crate::error::Result {
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
        _state: &mut crate::layout::DynamicLayoutState<'data, Self>,
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _resources: &crate::layout::FinaliseLayoutResources<'_, 'data, Self>,
        _resolutions_out: &mut crate::layout::ResolutionWriter<Self>,
    ) -> crate::error::Result<Option<Self::DynamicLayoutExt<'data>>> {
        Ok(None)
    }

    fn take_dynsym_index(
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _section_layouts: &crate::output_section_map::OutputSectionMap<
            crate::layout::OutputRecordLayout,
        >,
    ) -> crate::error::Result<u32> {
        crate::bail!("Wasm dynamic symbol table is not emitted")
    }

    fn compute_object_addresses<'data>(
        _object: &crate::layout::ObjectLayoutState<'data, Self>,
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) {
    }

    fn layout_resources_ext<'data>(
        _groups: &[crate::grouping::Group<'data, Self>],
    ) -> Self::LayoutResourcesExt<'data> {
    }

    fn gc_unit_for_symbol<'data>(
        object: &Self::File<'data>,
        symbol: &Self::SymtabEntry,
        _symbol_index: object::SymbolIndex,
    ) -> crate::error::Result<Option<Self::GcUnit>> {
        Ok(wasm_gc_unit_for_symbol(object, symbol))
    }

    fn activate_object_gc<'data, 'scope, A: platform::Arch<Platform = Self>>(
        object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        resources: &'scope crate::layout::GraphResources<'data, 'scope, Self>,
        queue: &mut crate::layout::LocalWorkQueue<Self>,
        scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result {
        object.format_specific.ensure_gc_states(object.object);
        if resources.symbol_db.args.should_gc_sections() {
            enqueue_wasm_gc_roots::<A>(object, resources, queue, scope)?;
        } else {
            mark_all_wasm_units_live_and_scan_relocs::<A>(object, resources, queue, scope)?;
        }
        Ok(())
    }

    fn load_gc_unit<'data, 'scope, A: platform::Arch<Platform = Self>>(
        object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        resources: &'scope crate::layout::GraphResources<'data, 'scope, Self>,
        queue: &mut crate::layout::LocalWorkQueue<Self>,
        unit: Self::GcUnit,
        scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result {
        if !object.format_specific.mark_live(unit) {
            return Ok(());
        }

        match unit {
            WasmGcUnit::DefinedFunction(_) | WasmGcUnit::DataSegment(_) => {
                object
                    .format_specific
                    .ensure_relocs_decoded(object.object)?;
                walk_wasm_gc_unit_edges::<A>(object, unit, resources, queue, scope)?;
            }
            WasmGcUnit::FunctionImport(_) | WasmGcUnit::GlobalImport(_) => {
                note_wasm_import_unit_definition::<A>(object, unit, resources, queue, scope);
            }
            WasmGcUnit::DefinedGlobal(_) => {}
        }
        Ok(())
    }

    fn load_object_section_relocations<'data, 'scope, A: platform::Arch<Platform = Self>>(
        _state: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _queue: &mut crate::layout::LocalWorkQueue<Self>,
        _resources: &'scope crate::layout::GraphResources<'data, '_, Self>,
        _section: crate::layout::Section,
        _section_index: object::SectionIndex,
        _scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn create_dynamic_symbol_definition<'data>(
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        _symbol_id: crate::symbol_db::SymbolId,
    ) -> crate::error::Result<crate::layout::DynamicSymbolDefinition<'data, Self>> {
        crate::bail!("Wasm dynamic symbol definitions are not emitted")
    }

    fn update_segment_keep_list(
        _program_segments: &crate::program_segments::ProgramSegments<Self::ProgramSegmentDef>,
        _keep_segments: &mut [bool],
        _args: &Self::Args,
    ) {
    }

    fn program_segment_defs() -> &'static [Self::ProgramSegmentDef] {
        PROGRAM_SEGMENT_DEFS
    }

    fn unconditional_segment_defs() -> &'static [Self::ProgramSegmentDef] {
        &[]
    }

    fn program_segment_should_include_section(
        segment_def: Self::ProgramSegmentDef,
        _section_info: &crate::output_section_id::SectionOutputInfo<Self>,
        section_id: crate::output_section_id::OutputSectionId,
        _rosegment: bool,
    ) -> bool {
        use crate::wasm::output_section_id as osid;

        let section_segment_type = match section_id {
            crate::output_section_id::FILE_HEADER => SegmentType::Header,
            osid::WASM_TYPE
            | osid::WASM_IMPORT
            | osid::WASM_FUNCTION
            | osid::WASM_TABLE
            | osid::WASM_MEMORY
            | osid::WASM_GLOBAL
            | osid::WASM_EXPORT
            | osid::WASM_START
            | osid::WASM_ELEMENT
            | osid::WASM_DATA_COUNT
            | osid::WASM_CODE
            | osid::WASM_DATA
            | osid::WASM_NAME
            | osid::WASM_TARGET_FEATURES => SegmentType::Module,
            _ => SegmentType::Unused,
        };

        segment_def.segment_type == section_segment_type
    }

    fn create_linker_defined_symbols(
        symbols: &mut crate::parsing::InternalSymbolsBuilder<Self>,
        _output_kind: crate::output_kind::OutputKind,
        _args: &Self::Args,
    ) {
        // Reserve SymbolId 0 as the linker’s undefined sentinel (Wasm objects have no null symbol
        // entry).
        symbols
            .add_symbol(crate::parsing::InternalSymDefInfo::new(
                crate::parsing::SymbolPlacement::Undefined,
                b"",
            ))
            .hide();

        for sym in <WasmLinkerSymbol as strum::IntoEnumIterator>::iter() {
            symbols.platform_specific(sym.name(), sym).hide();
        }
    }

    fn built_in_section_infos<'data>()
    -> Vec<crate::output_section_id::SectionOutputInfo<'data, Self>> {
        SECTION_DEFINITIONS
            .iter()
            .map(|d| crate::output_section_id::SectionOutputInfo {
                section_attributes: SectionAttributes::default(),
                kind: d.kind,
                min_alignment: crate::alignment::MIN,
                location_info: None,
                secondary_order: None,
                region_name: None,
                fill: None,
                phdrs: Vec::new(),
                input_order: false,
            })
            .collect()
    }

    fn new_resolved_object_ext<'data>(
        symbol_id_range: crate::symbol_db::SymbolIdRange,
        file_id: crate::input_data::FileId,
    ) -> Self::ResolvedObjectExt<'data> {
        WasmObjectLayout {
            symbol_id_range,
            file_id,
            gc_states_ready: false,
            gc_defined_functions: Vec::new(),
            gc_defined_globals: Vec::new(),
            gc_data_segments: Vec::new(),
            gc_function_imports: Vec::new(),
            gc_global_imports: Vec::new(),
            func_import_symbol_offsets: Vec::new(),
            global_import_symbol_offsets: Vec::new(),
            relocs_ready: false,
            code_relocations: Vec::new(),
            data_relocations: Vec::new(),
            function_bodies: Vec::new(),
            data_segments: Vec::new(),
            function_body_spans: Vec::new(),
            data_segment_spans: Vec::new(),
            defined_function_live_ordinal: Vec::new(),
            defined_global_live_ordinal: Vec::new(),
        }
    }

    fn new_object_layout_state_ext<'data>(
        input: Self::ResolvedObjectExt<'data>,
    ) -> Self::ObjectLayoutStateExt<'data> {
        input
    }

    fn create_finalise_sizes_ext<'data, 'states, 'files, A: platform::Arch<Platform = Self>>(
        _args: &Self::Args,
        groups: &'files mut [layout::GroupState<'data, Self>],
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) -> crate::error::Result<Self::FinaliseSizesExt<'data>>
    where
        'data: 'files,
        'data: 'states,
    {
        build_output_module_layout(groups, symbol_db)
    }

    fn create_layout_ext<'data>(
        finalise_sizes_ext: Self::FinaliseSizesExt<'data>,
        _resolutions: &layout::SymbolResolutions<Self>,
    ) -> Result<Self::LayoutExt<'data>> {
        Ok(finalise_sizes_ext)
    }

    fn load_exception_frame_data<'data, 'scope, A: platform::Arch<Platform = Self>>(
        _object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _eh_frame_section_index: object::SectionIndex,
        _resources: &'scope crate::layout::GraphResources<'data, '_, Self>,
        _queue: &mut crate::layout::LocalWorkQueue<Self>,
        _scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result {
        // Wasm doesn't have ELF-style `.eh_frame`.
        Ok(())
    }

    fn non_empty_section_loaded<'data, 'scope, A: platform::Arch<Platform = Self>>(
        _object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _queue: &mut crate::layout::LocalWorkQueue<Self>,
        _unloaded: crate::resolution::UnloadedSection,
        _resources: &'scope crate::layout::GraphResources<'data, 'scope, Self>,
        _scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn new_epilogue_layout<'data>(
        _args: &Self::Args,
        _output_kind: crate::output_kind::OutputKind,
        _dynamic_symbol_definitions: &mut [crate::layout::DynamicSymbolDefinition<'data, Self>],
        _group_states: &[layout::GroupState<'data, Self>],
    ) -> Self::EpilogueLayoutExt {
    }

    fn apply_non_addressable_indexes_epilogue(
        _counts: &mut Self::NonAddressableCounts,
        _state: &mut Self::EpilogueLayoutExt,
    ) {
        // No-op: Wasm has no version table.
    }

    fn apply_non_addressable_indexes<'data, 'groups>(
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        _counts: &Self::NonAddressableCounts,
        _mem_sizes_iter: impl Iterator<
            Item = &'groups mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        >,
    ) {
        // Wasm has no non-addressable side tables.
    }

    fn finalise_sizes_epilogue<'data>(
        _state: &mut Self::EpilogueLayoutExt,
        mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _dynamic_symbol_definitions: &[crate::layout::DynamicSymbolDefinition<'data, Self>],
        properties: &Self::LayoutExt<'data>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) {
        properties.encoded_sections.add_sizes_to(mem_sizes);
        properties.add_code_section_size(mem_sizes);
        properties.add_data_section_size(mem_sizes);
    }

    fn finalise_sizes_all<'data>(
        _mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) {
    }

    fn finalise_layout_epilogue<'data>(
        _epilogue_state: &mut Self::EpilogueLayoutExt,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        common_state: &Self::LayoutExt<'data>,
        _dynsym_start_index: u32,
        _dynamic_symbol_defs: &[crate::layout::DynamicSymbolDefinition<Self>],
    ) -> crate::error::Result {
        common_state.encoded_sections.add_sizes_to(memory_offsets);
        common_state.add_code_section_size(memory_offsets);
        common_state.add_data_section_size(memory_offsets);
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
        // No dynamic linking yet, so nothing can be interposed.
        true
    }

    fn allocate_header_sizes<'data>(
        _prelude: &mut crate::layout::PreludeLayoutState<'data, Self>,
        sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _header_info: &crate::layout::HeaderInfo,
        _program_segments: &crate::program_segments::ProgramSegments<Self::ProgramSegmentDef>,
        _output_sections: &crate::output_section_id::OutputSections<Self>,
        _resources: &layout::FinaliseSizesResources<'data, '_, Self>,
        _args: &Self::Args,
    ) {
        sizes.increment(crate::part_id::FILE_HEADER, (WASM_MAGIC.len() + 4) as u64);
    }

    fn finalise_sizes_for_symbol<'data>(
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        _symbol_id: crate::symbol_db::SymbolId,
        _flags: crate::value_flags::ValueFlags,
    ) -> crate::error::Result {
        Ok(())
    }

    fn allocate_resolution(
        _flags: crate::value_flags::ValueFlags,
        _mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _output_kind: crate::output_kind::OutputKind,
        _args: &Self::Args,
    ) {
    }

    fn allocate_object_symtab_space<'data>(
        _state: &crate::layout::ObjectLayoutState<'data, Self>,
        _common: &mut crate::layout::CommonGroupState<'data, Self>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        _per_symbol_flags: &crate::value_flags::AtomicPerSymbolFlags,
    ) -> crate::error::Result {
        Ok(())
    }

    fn allocate_internal_symbol(
        _symbol_id: crate::symbol_db::SymbolId,
        _def_info: &crate::parsing::InternalSymDefInfo<Self>,
        _sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _symbol_db: &crate::symbol_db::SymbolDb<Self>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn allocate_prelude(
        _common: &mut crate::layout::CommonGroupState<Self>,
        _symbol_db: &crate::symbol_db::SymbolDb<Self>,
    ) {
    }

    fn finalise_prelude_layout<'data>(
        _prelude: &crate::layout::PreludeLayoutState<Self>,
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _resources: &crate::layout::FinaliseLayoutResources<'_, 'data, Self>,
    ) -> crate::error::Result<Self::PreludeLayoutExt> {
        Ok(())
    }

    fn create_resolution(
        flags: crate::value_flags::ValueFlags,
        raw_value: u64,
        dynamic_symbol_index: Option<std::num::NonZeroU32>,
        _memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        _args: &<Self as crate::platform::Platform>::Args,
        _output_kind: crate::OutputKind,
    ) -> crate::layout::Resolution<Self> {
        crate::layout::Resolution {
            raw_value,
            dynamic_symbol_index,
            flags,
            format_specific: (),
        }
    }

    fn raw_symbol_name<'data>(
        name_bytes: &'data [u8],
        _verneed_table: &Self::VerneedTable<'data>,
        _symbol_index: object::SymbolIndex,
    ) -> Self::RawSymbolName<'data> {
        RawSymbolName { name: name_bytes }
    }

    fn default_layout_rules(_args: &Self::Args) -> Vec<SectionRule<'static>> {
        DEFAULT_SECTION_RULES.to_vec()
    }

    fn align_load_segment_start(
        _segment_def: Self::ProgramSegmentDef,
        _segment_alignment: crate::alignment::Alignment,
        _file_offset: &mut usize,
        _mem_offset: &mut u64,
    ) {
        // Wasm has no load segments in the ELF sense.
    }

    fn build_output_order_and_program_segments<'data>(
        _custom: &crate::output_section_id::CustomSectionIds,
        output_kind: crate::output_kind::OutputKind,
        output_sections: &crate::output_section_id::OutputSections<'data, Self>,
        secondary: &crate::output_section_map::OutputSectionMap<
            Vec<crate::output_section_id::OutputSectionId>,
        >,
        _location_counters: &[crate::layout_rules::LocationCounter<'data>],
    ) -> (
        crate::output_section_id::OutputOrder<'data>,
        crate::program_segments::ProgramSegments<Self::ProgramSegmentDef>,
    ) {
        use crate::wasm::output_section_id as osid;

        let mut builder = crate::output_section_id::OutputOrderBuilder::<Self>::new(
            Self::program_segment_defs().to_vec(),
            output_kind,
            output_sections,
            secondary,
            false,
            &[],
        );

        builder.add_section(crate::output_section_id::FILE_HEADER);
        builder.add_section(osid::WASM_TYPE);
        builder.add_section(osid::WASM_IMPORT);
        builder.add_section(osid::WASM_FUNCTION);
        builder.add_section(osid::WASM_TABLE);
        builder.add_section(osid::WASM_MEMORY);
        builder.add_section(osid::WASM_GLOBAL);
        builder.add_section(osid::WASM_EXPORT);
        builder.add_section(osid::WASM_START);
        builder.add_section(osid::WASM_ELEMENT);
        builder.add_section(osid::WASM_DATA_COUNT);
        builder.add_section(osid::WASM_CODE);
        builder.add_section(osid::WASM_DATA);
        builder.add_section(osid::WASM_NAME);
        builder.add_section(osid::WASM_TARGET_FEATURES);

        builder.build()
    }

    fn default_symtab_entry() -> Self::SymtabEntry {
        WasmSymbol::default()
    }

    fn is_allowed_in_archive(kind: crate::file_kind::FileKind) -> bool {
        kind == crate::file_kind::FileKind::WasmObject
    }

    fn section_identity<'data>(
        name: SectionName<'data>,
        _section: &Self::SectionHeader,
    ) -> SectionIdentity<'data, Self> {
        SectionIdentity::new(name, ())
    }

    fn section_identity_from_name<'data>(
        name: SectionName<'data>,
    ) -> Option<SectionIdentity<'data, Self>> {
        Some(SectionIdentity::new(name, ()))
    }
}
