use super::ELF_NUM_BUILT_IN_REGULAR_SECTIONS;
use super::ELF_NUM_SINGLE_PART_SECTIONS;
use super::THUNK_SYMBOL_PREFIX;
#[allow(unused_imports)]
use super::file::*;
#[allow(unused_imports)]
use super::gnu::*;
#[allow(unused_imports)]
use super::output::*;
use super::output_section_id;
use super::part_id;
#[allow(unused_imports)]
use super::strtab::*;
#[allow(unused_imports)]
use super::types::*;
use crate::FileSystem;
use crate::alignment::Alignment;
use crate::arch::Architecture;
use crate::args::BSymbolicKind;
use crate::args::RelocationModel;
use crate::args::elf::BuildIdOption;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::elf_writer;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::expression_eval;
use crate::file_kind::FileKind;
use crate::gdb_index::InputDebugIndexSection;
use crate::grouping::SequencedLinkerScript;
use crate::layout;
use crate::layout::CommonGroupState;
use crate::layout::DynamicSymbolDefinition;
use crate::layout::HandlerData as _;
use crate::layout::ObjectLayoutState;
use crate::layout::OutputRecordLayout;
use crate::layout::Resolution;
use crate::layout::SectionGcUnit;
use crate::layout::SymbolCopyInfo;
use crate::layout_rules::SectionRule;
use crate::layout_rules::SectionRuleOutcome;
use crate::linker_script;
use crate::output_kind::OutputKind;
use crate::output_section_id::CustomSectionIds;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputOrderBuilder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_id::SectionIdentity;
use crate::output_section_id::SectionName;
use crate::output_section_id::SectionOutputInfo;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::SymbolPlacement;
use crate::platform;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::ProgramSegmentDef as _;
use crate::platform::RawSymbolName as _;
use crate::platform::RelocationSequence;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionFlags as _;
use crate::platform::SectionHeader as _;
use crate::platform::SectionType as _;
use crate::platform::Symbol as _;
use crate::platform::ThunkConfig;
use crate::platform::VerneedTable as _;
use crate::program_segments::ProgramSegmentId;
use crate::program_segments::ProgramSegments;
use crate::program_segments::SegmentEntry;
use crate::resolution::LoadedMetrics;
use crate::resolution::SectionSlot;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::version_script::VersionScript;
use crate::writable_elf::WritableSymbol;
use hashbrown::HashMap;
use itertools::Itertools as _;
use linker_utils::elf::SectionFlags;
use linker_utils::elf::SectionType;
use linker_utils::elf::SegmentFlags;
use linker_utils::elf::SegmentType;
use linker_utils::elf::pf;
use linker_utils::elf::pt;
use linker_utils::elf::secnames;
use linker_utils::elf::shf;
use linker_utils::elf::sht;
use object::LittleEndian;
use object::read::elf::RelocationSections;
use object::read::elf::SectionHeader as _;
use rayon::Scope;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::num::NonZeroU64;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;

impl<C: ElfClass> platform::Platform for Elf<C> {
    const NUM_SINGLE_PART_SECTIONS: u32 = ELF_NUM_SINGLE_PART_SECTIONS;
    const NUM_BUILT_IN_REGULAR_SECTIONS: usize = ELF_NUM_BUILT_IN_REGULAR_SECTIONS;

    const TEXT_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::TEXT);
    const DATA_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::DATA);
    const BSS_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::BSS);
    const RODATA_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::RODATA);
    const TDATA_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::TDATA);
    const TBSS_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::TBSS);
    const STRTAB_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::STRTAB);
    const SYMTAB_GLOBAL_SECTION_ID: Option<OutputSectionId> =
        Some(output_section_id::SYMTAB_GLOBAL);
    const GOT_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::GOT);
    const PLT_GOT_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::PLT_GOT);
    const SYMTAB_LOCAL_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::SYMTAB_LOCAL);
    const SYMTAB_SHNDX_LOCAL_SECTION_ID: Option<OutputSectionId> =
        Some(output_section_id::SYMTAB_SHNDX_LOCAL);
    const SYMTAB_SHNDX_GLOBAL_SECTION_ID: Option<OutputSectionId> =
        Some(output_section_id::SYMTAB_SHNDX_GLOBAL);
    const GDB_INDEX_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::GDB_INDEX);
    const DYNSTR_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::DYNSTR);
    const DYNSYM_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::DYNSYM);
    const EH_FRAME_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::EH_FRAME);
    const NOTE_GNU_PROPERTY_SECTION_ID: Option<OutputSectionId> =
        Some(output_section_id::NOTE_GNU_PROPERTY);
    const NOTE_GNU_BUILD_ID_SECTION_ID: Option<OutputSectionId> =
        Some(output_section_id::NOTE_GNU_BUILD_ID);
    const RISCV_ATTRIBUTES_SECTION_ID: Option<OutputSectionId> =
        Some(output_section_id::RISCV_ATTRIBUTES);
    const GOT_RELR_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::GOT_RELR);
    const GNU_VERSION_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::GNU_VERSION);
    const COMMENT_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::COMMENT);
    const INTERP_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::INTERP);
    const SFRAME_SECTION_ID: Option<OutputSectionId> = Some(output_section_id::SFRAME);
    const RELRO_PADDING_SECTION_ID: Option<OutputSectionId> =
        Some(output_section_id::RELRO_PADDING);

    const CUSTOM_PHDR_EXCLUDED_SECTION_IDS: &'static [OutputSectionId] = &[
        output_section_id::PROGRAM_HEADERS,
        output_section_id::SECTION_HEADERS,
        // GNU ld only emits RELRO padding when the script has a GNU_RELRO phdr.
        output_section_id::RELRO_PADDING,
    ];

    /// The `.init` section `crti.o` contains the start of a function and `crtn.o` contains the end
    /// of that function. If `.init` has say alignment = 4 and we add padding after it to bring it
    /// up to a multiple of 4 bytes, then we'll break the function, since the padding bytes won't be
    /// valid instructions. Same thing applies to `.fini`.
    const PACKED_SECTION_IDS: &'static [OutputSectionId] =
        &[output_section_id::INIT, output_section_id::FINI];

    const VERIFY_IGNORE_ALIGNMENT_SECTION_IDS: &'static [OutputSectionId] = &[
        output_section_id::GNU_HASH,
        output_section_id::EH_FRAME,
        output_section_id::GNU_VERSION_D,
        output_section_id::STRTAB,
    ];

    const VERIFY_IGNORE_SECTION_IDS: &'static [OutputSectionId] = &[
        output_section_id::RELA_PLT,
        output_section_id::EH_FRAME_HDR,
        output_section_id::RELA_DYN_GENERAL,
        output_section_id::RELA_DYN_RELATIVE,
        output_section_id::RELR_DYN,
        output_section_id::GNU_VERSION,
        output_section_id::GNU_HASH,
        output_section_id::DYNAMIC,
        crate::output_section_id::FILE_HEADER,
        output_section_id::PROGRAM_HEADERS,
        output_section_id::SECTION_HEADERS,
        output_section_id::SHSTRTAB,
    ];

    const HAS_NULL_SYMBOL_ENTRY: bool = true;

    type File<'data> = File<'data, C>;
    type FileFlags = object::elf::FileFlags;
    type SymtabEntry = SymtabEntry<C>;
    type PlatformSpecificSymbol = core::convert::Infallible;
    type SectionHeader = SectionHeader<C>;
    type SectionFlags = SectionFlags;
    type SectionAttributes = SectionAttributes<C>;
    type SectionType = SectionType;
    type SegmentType = SegmentType;
    type ProgramSegmentDef = ProgramSegmentDef;
    type BuiltInSectionDetails = BuiltInSectionDetails<C>;
    type RelocationSections = RelocationSections;
    type DynamicEntry = DynamicEntry<C>;
    type DynamicSymbolDefinitionExt = DynamicSymbolDefinitionExt;
    type RelocationInfo = object::elf::RelocationType;
    type FinaliseSizesExt<'data> = LayoutExt;
    type LayoutExt<'data> = LayoutExt;
    type SymbolVersionIndex = Versym;
    type NonAddressableCounts = NonAddressableCounts;
    type NonAddressableIndexes = NonAddressableIndexes;
    type EpilogueLayoutExt = EpilogueLayoutExt;
    type GroupLayoutExt = GroupLayoutExt;
    type CommonGroupStateExt = CommonGroupStateExt;
    type StubLibraryLayoutStateExt = ();
    type StubLibraryLayoutExt = ();
    type PreludeLayoutStateExt = PreludeLayoutStateExt;
    type PreludeLayoutExt = PreludeLayoutExt;
    type ArchIdentifier = object::elf::Machine;
    type SectionIterator<'a> = core::slice::Iter<'a, SectionHeader<C>>;
    type DynamicTagValues<'data> = crate::elf::DynamicTagValues<'data>;
    type RelocationList<'data> = RelocationList<'data, C>;
    type VersionNames<'data> = VersionNames<'data>;
    type RawSymbolName<'data> = RawSymbolName<'data>;
    type VerneedTable<'data> = VerneedTable<'data>;
    type ObjectLayoutStateExt<'data> = ObjectLayoutStateExt<'data, C>;
    type DynamicLayoutStateExt<'data> = DynamicLayoutStateExt<'data, C>;
    type DynamicLayoutExt<'data> = DynamicLayoutExt<'data, C>;
    type LayoutResourcesExt<'data> = LayoutResourcesExt<'data>;
    type Args = ElfArgs;
    type ResolutionExt = ResolutionExt;
    type SymtabShndxEntry = SymtabShndxEntry;
    type ResolvedObjectExt<'data> = ResolvedObjectExt<'data>;
    type SectionIdentityExt = ();
    type GcUnit = SectionGcUnit;
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

    fn write_output_file<'data, A: Arch<Platform = Self>, F: FileSystem>(
        output: &crate::file_writer::Output<F>,
        layout: &layout::Layout<'data, Self>,
    ) -> Result {
        output.write(layout, elf_writer::write::<C, A>)
    }

    fn maybe_compress_debug_sections<'data, A: Arch<Platform = Self>>(
        layout: &mut layout::Layout<'data, Self>,
    ) -> Result {
        crate::compression::maybe_compress_debug_sections_elf::<C, A>(layout)
    }

    fn maybe_init_linker_plugin<'data>(
        args: &'data Self::Args,
        linker_plugin_arena: &'data colosseum::sync::Arena<crate::linker_plugins::LoadedPlugin>,
        herd: &'data bumpalo_herd::Herd,
    ) -> Result<Option<crate::linker_plugins::LinkerPlugin<'data>>> {
        crate::linker_plugins::LinkerPlugin::from_args::<C>(args, linker_plugin_arena, herd)
    }

    fn plugin_all_symbols_read<'data, F: FileSystem>(
        plugin: &mut crate::linker_plugins::LinkerPlugin<'data>,
        symbol_db: &mut SymbolDb<'data, Self>,
        resolver: &mut crate::resolution::Resolver<'data, Self>,
        file_loader: &mut crate::input_data::FileLoader<'data, F>,
        per_symbol_flags: &mut crate::value_flags::PerSymbolFlags,
        output_sections: &mut OutputSections<'data, Self>,
        layout_rules_builder: &mut crate::layout_rules::LayoutRulesBuilder<'data>,
    ) -> Result {
        plugin.all_symbols_read(
            symbol_db,
            resolver,
            file_loader,
            per_symbol_flags,
            output_sections,
            layout_rules_builder,
        )
    }

    fn resolve_lto_symbols<'data, 'scope>(
        obj: &crate::linker_plugins::LtoInput<'data>,
        resources: &'scope crate::resolution::ResolutionResources<'data, 'scope, Self>,
        definitions_out: &mut [SymbolId],
        scope: &Scope<'scope>,
    ) -> Result {
        crate::linker_plugins::resolve_lto_symbols(obj, resources, definitions_out, scope)
    }

    fn apply_force_keep_sections(keep_sections: &mut OutputSectionMap<bool>, args: &ElfArgs) {
        // Some of these sections aren't really empty, but we just haven't allocated space for them
        // yet. e.g. we don't allocate space for section headers until we know which sections we're
        // keeping, which by inherently needs to be after this method is called.
        const FORCE_KEEP_SECTIONS: &[OutputSectionId] = &[
            crate::output_section_id::FILE_HEADER,
            output_section_id::PROGRAM_HEADERS,
            output_section_id::SECTION_HEADERS,
            output_section_id::SHSTRTAB,
        ];

        for section_id in FORCE_KEEP_SECTIONS {
            *keep_sections.get_mut(*section_id) = true;
        }

        // Keep .relro_padding unless relro is disabled.
        if args.relro {
            *keep_sections.get_mut(output_section_id::RELRO_PADDING) = true;
        }
    }

    fn is_zero_sized_section_content(section_id: OutputSectionId) -> bool {
        // We always consider empty sections as content except for sframe sections.
        section_id != output_section_id::SFRAME
    }

    fn built_in_section_details() -> &'static [Self::BuiltInSectionDetails] {
        &Self::SECTION_DEFINITIONS
    }

    fn section_attributes(header: &Self::SectionHeader) -> Self::SectionAttributes {
        SectionAttributes {
            flags: header.sh_flags(LittleEndian),
            ty: header.sh_type(LittleEndian),
            entsize: header.sh_entsize(LittleEndian).into(),
            overrides: Default::default(),
            received_input_flags: false,
            class: PhantomData,
        }
    }

    fn validate_sizes(mem_sizes: &OutputSectionPartMap<u64>) -> Result {
        if mem_sizes.get(part_id::GNU_VERSION) > 0 {
            let num_dynamic_symbols = mem_sizes.get(part_id::DYNSYM) / C::SYMTAB_ENTRY_SIZE;
            let num_versym = mem_sizes.get(part_id::GNU_VERSION) / size_of::<Versym>() as u64;
            if num_versym != num_dynamic_symbols {
                bail!(
                    "Object has {num_dynamic_symbols} dynamic symbols, but \
                         has {num_versym} versym entries"
                );
            }
        }

        Ok(())
    }

    fn finalise_group_layout(memory_offsets: &OutputSectionPartMap<u64>) -> Self::GroupLayoutExt {
        GroupLayoutExt {
            eh_frame_start_address: memory_offsets.get(part_id::EH_FRAME),
        }
    }

    fn frame_data_base_address(memory_offsets: &OutputSectionPartMap<u64>) -> u64 {
        // References to symbols defined in .eh_frame are a bit weird, since it's a section where
        // we're GCing stuff, but crtbegin.o and crtend.o use them in order to find the start and
        // end of the whole .eh_frame section.
        memory_offsets.get(part_id::EH_FRAME)
    }

    fn post_gc<'data>(
        groups: &mut [layout::GroupState<Elf<C>>],
        symbol_db: &SymbolDb<'data, Elf<C>>,
    ) -> Result {
        tracing::debug!(target: "metrics", total = groups
            .iter()
            .map(|g| g.common.format_specific.exception_frame_count)
            .sum::<usize>(), "exception frames");

        tracing::debug!(target: "metrics", section = "`.eh_frame`", relocations = groups
            .iter()
            .map(|g| g.common.format_specific.exception_frame_relocations)
            .sum::<usize>(), "resolved relocations");

        if symbol_db.args.is_relr_enabled() {
            load_glibc_abi_dt_relr_version(groups, symbol_db)?;
        }

        Ok(())
    }

    fn activate_dynamic<'data>(
        state: &mut layout::DynamicLayoutState<'data, Self>,
        common: &mut CommonGroupState<'data, Self>,
    ) {
        common.allocate(part_id::DYNAMIC, C::DYNAMIC_ENTRY_SIZE);

        common.allocate(part_id::DYNSTR, state.lib_name.len() as u64 + 1);

        state.format_specific.symbol_versions_needed = vec![false; state.object.verdefnum as usize];
    }

    fn pre_finalise_sizes_prelude<'scope, 'data>(
        prelude: &mut layout::PreludeLayoutState<'data, Self>,
        common: &mut layout::CommonGroupState<'data, Self>,
        resources: &layout::GraphResources<'data, 'scope, Self>,
    ) {
        if resources
            .layout_resources_ext
            .uses_tlsld
            .load(atomic::Ordering::Relaxed)
        {
            // Allocate space for a TLS module number and offset for use with TLSLD relocations.
            common.allocate(part_id::GOT, C::GOT_ENTRY_SIZE * 2);
            prelude.format_specific.needs_tlsld_got_entry = true;
            // For shared objects, we'll need to use a DTPMOD relocation to fill in the TLS module
            // number.
            if !resources.symbol_db.output_kind.is_executable() {
                common.allocate(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
            }
        }
    }

    fn finalise_sizes_dynamic<'data>(
        object: &mut layout::DynamicLayoutState<'data, Self>,
        common: &mut layout::CommonGroupState<'data, Self>,
    ) -> Result {
        allocate_for_copy_relocations(object, common)
    }

    fn finalise_object_sizes<'data>(
        object: &mut layout::ObjectLayoutState<'data, Elf<C>>,
        common: &mut layout::CommonGroupState<'data, Elf<C>>,
    ) {
        // TODO: Deduplicate CIEs from different objects, then only allocate space for those CIEs
        // that we "won".
        for cie in &object.format_specific.cies {
            object.format_specific.eh_frame_size += cie.cie.bytes.len() as u64;
        }
        common.allocate(part_id::EH_FRAME, object.format_specific.eh_frame_size);
    }

    fn finalise_object_layout<'data>(
        object: &layout::ObjectLayoutState<'data, Elf<C>>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
    ) {
        memory_offsets.increment(part_id::EH_FRAME, object.format_specific.eh_frame_size);
    }

    fn file_thunk_config<'data>(file: &File<'data, C>) -> Option<ThunkConfig> {
        thunk_config_for_object(file)
    }

    fn finalise_layout_dynamic<'data>(
        state: &mut Self::DynamicLayoutState<'data>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resources: &Self::FinaliseLayoutResources<'_, 'data>,
        resolutions_out: &mut Self::ResolutionWriter<'_, '_>,
    ) -> Result<Option<Self::DynamicLayoutExt<'data>>> {
        let mut is_last_verneed = false;

        if let Some(v) = &state.format_specific.verneed_info
            && v.version_count > 0
        {
            memory_offsets.increment(
                part_id::GNU_VERSION_R,
                size_of::<crate::elf::Verneed>() as u64
                    + u64::from(v.version_count) * size_of::<crate::elf::Vernaux>() as u64,
            );

            let version_r_layout = resources
                .section_layouts
                .get(output_section_id::GNU_VERSION_R);

            is_last_verneed = memory_offsets.get(part_id::GNU_VERSION_R)
                == version_r_layout.mem_offset + version_r_layout.mem_size;
        }

        let version_mapping = compute_version_mapping(
            &state.format_specific.symbol_versions_needed,
            state.format_specific.non_addressable_indexes,
        );

        let copy_relocation_symbols = state
            .format_specific
            .copy_relocations
            .values()
            .map(|info| info.symbol_id)
            // We'll write the copy relocations in this order, so we need to sort it to ensure
            // deterministic output.
            .sorted()
            .collect_vec();

        let copy_relocation_addresses =
            assign_copy_relocation_addresses(state, &copy_relocation_symbols, memory_offsets)?;

        for (local_symbol, &flags) in state.object.symbols_iter().zip(
            resources
                .per_symbol_flags
                .raw_range(state.symbol_id_range()),
        ) {
            let flags = flags.get();

            if !flags.has_resolution() {
                resolutions_out.write(None)?;
                continue;
            }

            let address;
            let dynamic_symbol_index;

            if flags.needs_copy_relocation() || flags.needs_canonical_plt() {
                address = if flags.needs_copy_relocation() {
                    let input_address = local_symbol.value();
                    *copy_relocation_addresses
                        .get(&input_address)
                        .context("Internal error: Missing copy relocation address")?
                } else {
                    0
                };

                // Since this is a definition, the dynamic symbol index will be determined by the
                // epilogue and set by `update_dynamic_symbol_resolutions`.
                dynamic_symbol_index = None;
            } else {
                address = 0;
                let symbol_index =
                    Self::take_dynsym_index(memory_offsets, resources.section_layouts)?;

                dynamic_symbol_index = Some(
                    NonZeroU32::new(symbol_index)
                        .context("Tried to create dynamic symbol index 0")?,
                );
            }

            let resolution = Self::create_resolution(
                flags,
                address,
                dynamic_symbol_index,
                memory_offsets,
                resources.symbol_db.args,
                resources.symbol_db.output_kind,
            );

            resolutions_out.write(Some(resolution))?;
        }

        Ok(Some(DynamicLayoutExt {
            version_mapping,
            verneed_info: core::mem::take(&mut state.format_specific.verneed_info),
            is_last_verneed,
            copy_relocation_symbols,
        }))
    }

    fn compute_object_addresses<'data>(
        object: &layout::ObjectLayoutState<'data, Elf<C>>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
    ) {
        // Note, this is currently identical to finalise_object_layout above. The two functions are
        // however called separately and they might become different at some point.
        memory_offsets.increment(part_id::EH_FRAME, object.format_specific.eh_frame_size);
    }

    fn layout_resources_ext<'data>(groups: &[Self::Group<'data>]) -> LayoutResourcesExt<'data> {
        LayoutResourcesExt {
            sonames: Sonames::new(groups),
            uses_tlsld: AtomicBool::new(false),
        }
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

    const NEEDS_START_STOP_SECTION_GC: bool = true;

    fn gc_unit_for_section(section_index: object::SectionIndex) -> Self::GcUnit {
        SectionGcUnit::new(section_index)
    }

    fn activate_object_gc<'data, 'scope, A: Arch<Platform = Self>>(
        object: &mut layout::ObjectLayoutState<'data, Self>,
        common: &mut layout::CommonGroupState<'data, Self>,
        resources: &'scope layout::GraphResources<'data, 'scope, Self>,
        queue: &mut layout::LocalWorkQueue<Self>,
        scope: &Scope<'scope>,
    ) -> Result {
        object.activate_section_gc::<A>(common, resources, queue, scope)
    }

    fn load_gc_unit<'data, 'scope, A: Arch<Platform = Self>>(
        object: &mut layout::ObjectLayoutState<'data, Self>,
        common: &mut layout::CommonGroupState<'data, Self>,
        resources: &'scope layout::GraphResources<'data, 'scope, Self>,
        queue: &mut layout::LocalWorkQueue<Self>,
        unit: Self::GcUnit,
        scope: &Scope<'scope>,
    ) -> Result {
        object.handle_section_load_request::<A>(
            common,
            resources,
            queue,
            unit.section_index(),
            scope,
        )
    }

    fn load_object_section_relocations<'data, 'scope, A: Arch<Platform = Self>>(
        state: &mut layout::ObjectLayoutState<'data, Self>,
        common: &mut layout::CommonGroupState<'data, Self>,
        queue: &mut layout::LocalWorkQueue<Self>,
        resources: &'scope layout::GraphResources<'data, '_, Self>,
        _section: layout::Section,
        section_index: object::SectionIndex,
        scope: &Scope<'scope>,
    ) -> Result {
        if resources.symbol_db.args.should_output_partial_object() {
            return Ok(());
        }
        match state.relocations(section_index)? {
            RelocationList::Rela(relocations) => {
                load_section_relocations::<C, A, ElfRela<C>>(
                    state,
                    common,
                    queue,
                    resources,
                    section_index,
                    RelaSequence(relocations).rel_iter(),
                    scope,
                )?;
            }
            RelocationList::Crel(relocations) => {
                load_section_relocations::<C, A, ElfCrel<C>>(
                    state,
                    common,
                    queue,
                    resources,
                    section_index,
                    relocations.filter_map(|r| {
                        r.ok().map(|raw| ElfCrel {
                            raw,
                            class: PhantomData,
                        })
                    }),
                    scope,
                )?;
            }
        }

        Ok(())
    }

    fn load_associated_reloc_sections<'data, 'scope, A: Arch<Platform = Self>>(
        state: &mut layout::ObjectLayoutState<'data, Self>,
        common: &mut layout::CommonGroupState<'data, Self>,
        queue: &mut layout::LocalWorkQueue<Self>,
        resources: &'scope layout::GraphResources<'data, 'scope, Self>,
        section_index: object::SectionIndex,
        scope: &Scope<'scope>,
    ) -> Result {
        if !resources.symbol_db.args.emit_relocs() {
            return Ok(());
        }
        let e = LittleEndian;
        let target = u32::try_from(section_index.0).unwrap_or(u32::MAX);
        let reloc_indices: Vec<object::SectionIndex> = state
            .object
            .enumerate_sections()
            .filter(|(idx, header)| {
                *idx != section_index
                    && matches!(header.sh_type(e), sht::REL | sht::RELA)
                    && header.sh_info(e) == target
            })
            .map(|(idx, _)| idx)
            .collect();
        for idx in reloc_indices {
            if matches!(
                state.sections.get(idx.0),
                Some(SectionSlot::Unloaded(_) | SectionSlot::MustLoad(_))
            ) {
                state.handle_section_load_request::<A>(common, resources, queue, idx, scope)?;
            }
        }
        Ok(())
    }

    fn create_dynamic_symbol_definition<'data>(
        symbol_db: &Self::SymbolDb<'data>,
        symbol_id: SymbolId,
    ) -> Result<Self::DynamicSymbolDefinition<'data>> {
        let symbol_name = symbol_db.symbol_name(symbol_id)?;
        let RawSymbolName {
            name,
            version_name,
            is_default,
        } = RawSymbolName::parse(symbol_name.bytes());

        let mut version = object::elf::VER_NDX_GLOBAL.into();
        if (symbol_db.version_script.version_count() > 0 || version_name.is_some())
            && let Some(v) = symbol_db
                .version_script
                .version_for_symbol(&UnversionedSymbolName::prehashed(name), version_name)?
        {
            version = v.versym(!is_default);
        }
        Ok(layout::DynamicSymbolDefinition {
            symbol_id,
            name,
            format_specific: DynamicSymbolDefinitionExt {
                hash: object::elf::gnu_hash(name),
                version,
                is_version_node: false,
            },
        })
    }

    fn append_version_node_dynamic_symbols<'data>(
        dynamic_symbol_definitions: &mut Vec<DynamicSymbolDefinition<'data, Self>>,
        symbol_db: &SymbolDb<'data, Self>,
    ) {
        if !symbol_db.output_kind.needs_dynsym()
            || !symbol_db.output_kind.should_output_symbol_versions()
        {
            return;
        }
        let VersionScript::Regular(script) = &symbol_db.version_script else {
            return;
        };
        for (i, version) in script.version_iter().enumerate() {
            // Index 0 is the implicit BASE / soname version; GNU ld does not
            // emit a dynsym for it.
            if i == 0 || version.name.is_empty() {
                continue;
            }
            dynamic_symbol_definitions.push(DynamicSymbolDefinition {
                symbol_id: SymbolId::undefined(),
                name: version.name,
                format_specific: DynamicSymbolDefinitionExt {
                    hash: object::elf::gnu_hash(version.name),
                    version: (object::elf::VER_NDX_GLOBAL + i as u16).into(),
                    is_version_node: true,
                },
            });
        }
    }

    fn validate_section<'data>(
        section_info: &SectionOutputInfo<Elf<C>>,
        section_flags: SectionFlags,
        section_layout: &OutputRecordLayout,
        merge_target: OutputSectionId,
        output_sections: &OutputSections<'data, Elf<C>>,
        section_id: OutputSectionId,
    ) -> Result {
        // TODO: Remove the NOTE exception. Non-alloc sections should be placed outside of program
        // segments. NOTE sections are sometimes alloc and sometimes not. Alloc NOTE sections should
        // be placed within a LOAD segment and within a NOTE segment. Non-alloc NOTE sections
        // shouldn't be in any segment.

        // The .riscv.attributes section is non-alloc but is expected to be put into a
        // RISCV_ATTRIBUTES segment.
        if [sht::NOTE, sht::RISCV_ATTRIBUTES].contains(&section_info.section_attributes.ty) {
        } else if section_layout.mem_offset == 0
            && merge_target != crate::output_section_id::FILE_HEADER
        {
            // Sections with an explicit VMA of 0 (e.g. `.comment 0 :`) and empty
            // unused script sections can appear in PHDRS without a non-zero address.
        } else {
            // All segments should only cover sections that are allocated and have a non-zero
            // address.
            ensure!(
                section_layout.mem_offset != 0
                    || merge_target == crate::output_section_id::FILE_HEADER,
                "Missing memory offset for section {} present in a program segment.",
                output_sections.section_debug(section_id),
            );
            ensure!(
                section_flags.is_alloc() || section_info.location_info.is_some(),
                "Missing SHF_ALLOC section flag for section {} present in a program \
                         segment.",
                output_sections.section_debug(section_id)
            );
        }

        Ok(())
    }

    fn verify_resolution_allocation<A: Arch<Platform = Self>>(
        output_sections: &OutputSections<Elf<C>>,
        output_order: &OutputOrder,
        output_kind: OutputKind,
        mem_sizes: &OutputSectionPartMap<u64>,
        resolution: &layout::Resolution<Elf<C>>,
        args: &ElfArgs,
    ) -> Result {
        crate::elf_writer::verify_resolution_allocation::<C, A>(
            output_sections,
            output_order,
            output_kind,
            mem_sizes,
            resolution,
            args,
        )
    }

    fn update_segment_keep_list(
        program_segments: &ProgramSegments<ProgramSegmentDef>,
        keep_segments: &mut [bool],
        args: &ElfArgs,
    ) {
        // If relro is disabled, then discard the relro segment.
        if !args.relro {
            for (segment_def, keep) in program_segments.into_iter().zip(keep_segments.iter_mut()) {
                if segment_def.segment_type == pt::GNU_RELRO {
                    *keep = false;
                }
            }
        }

        // The PHDR program header should only be present if --nmagic is not set
        for (segment_def, keep) in program_segments.into_iter().zip(keep_segments.iter_mut()) {
            if segment_def.segment_type == pt::PHDR {
                *keep = !args.nmagic;
            }
            if segment_def.segment_type == pt::RISCV_ATTRIBUTES
                && args.arch != Architecture::RiscV64
            {
                *keep = false;
            }
        }
    }

    fn program_segment_defs() -> &'static [ProgramSegmentDef] {
        PROGRAM_SEGMENT_DEFS
    }

    fn phdr_flags_writable(flags: u64) -> bool {
        flags & u64::from(pf::WRITABLE.0) != 0
    }

    fn unconditional_segment_defs() -> &'static [ProgramSegmentDef] {
        &[STACK_SEGMENT_DEF]
    }

    fn program_segment_should_include_section(
        segment_def: ProgramSegmentDef,
        info: &SectionOutputInfo<Self>,
        section_id: OutputSectionId,
        rosegment: bool,
    ) -> bool {
        match segment_def.segment_type {
            pt::NOTE => info.section_attributes.ty == sht::NOTE,
            pt::TLS => info.section_attributes.flags.contains(shf::TLS),
            pt::LOAD => {
                let mut exec = info.section_attributes.flags.contains(shf::EXECINSTR);
                if !rosegment && !info.section_attributes.flags.contains(shf::WRITE) {
                    exec = true;
                }

                info.section_attributes.flags.contains(shf::ALLOC)
                    && info.section_attributes.flags.contains(shf::WRITE)
                        == segment_def.is_writable()
                    && exec == segment_def.is_executable()
            }
            pt::GNU_RELRO => {
                info.section_attributes.flags.contains(shf::TLS)
                    || section_id
                        .opt_built_in_details::<Self>()
                        .is_some_and(|details| details.is_relro)
            }
            other => section_id
                .opt_built_in_details::<Self>()
                .and_then(|details| details.target_segment_type)
                .is_some_and(|target_segment_type| target_segment_type == other),
        }
    }

    fn get_segment_flags_for_section(section_flags: &Self::SectionFlags) -> u32 {
        let mut flags = 0;
        if section_flags.contains(shf::ALLOC) {
            flags |= pf::READABLE.0;
        }
        if section_flags.contains(shf::WRITE) {
            flags |= pf::WRITABLE.0;
        }
        if section_flags.contains(shf::EXECINSTR) {
            flags |= pf::EXECUTABLE.0;
        }
        flags
    }

    fn create_linker_defined_symbols(
        symbols: &mut crate::parsing::InternalSymbolsBuilder<Elf<C>>,
        output_kind: OutputKind,
        args: &ElfArgs,
    ) {
        // The undefined symbol must always be symbol 0.
        symbols
            .add_symbol(InternalSymDefInfo::new(SymbolPlacement::Undefined, b""))
            .hide();

        // GNU ld PROVIDE_HIDDEN: define __ehdr_start only when referenced.
        symbols
            .add_symbol(
                InternalSymDefInfo::new(
                    SymbolPlacement::SectionStart(crate::output_section_id::FILE_HEADER),
                    b"__ehdr_start",
                )
                .with_provide(),
            )
            .hide();

        symbols.section_start(output_section_id::GOT, "_GLOBAL_OFFSET_TABLE_");

        // Don't emit .rela.plt start/stop symbols for static PIE executables. Doing so causes glibc
        // to call the resolver functions without taking into account that the binary has been
        // relocated.
        if output_kind != OutputKind::StaticExecutable(RelocationModel::PositionIndependent) {
            symbols
                .section_start(output_section_id::RELA_PLT, "__rela_iplt_start")
                .hide();
            symbols
                .section_end(output_section_id::RELA_PLT, "__rela_iplt_end")
                .hide();
        }

        symbols
            .section_start(output_section_id::PREINIT_ARRAY, "__preinit_array_start")
            .hide();
        symbols
            .section_group_end(output_section_id::PREINIT_ARRAY, "__preinit_array_end")
            .hide();

        symbols
            .section_start(output_section_id::INIT_ARRAY, "__init_array_start")
            .hide();
        symbols
            .section_group_end(output_section_id::INIT_ARRAY, "__init_array_end")
            .hide();

        symbols
            .section_start(output_section_id::FINI_ARRAY, "__fini_array_start")
            .hide();
        symbols
            .section_group_end(output_section_id::FINI_ARRAY, "__fini_array_end")
            .hide();

        // GNU ld doesn't emit these symbols in shared libraries, so we hide them
        let hidden = output_kind.is_shared_object();
        symbols
            .section_end(output_section_id::TEXT, "etext")
            .set_hidden(hidden);
        symbols
            .section_end(output_section_id::TEXT, "_etext")
            .set_hidden(hidden);
        symbols
            .section_end(output_section_id::TEXT, "__etext")
            .set_hidden(hidden);

        symbols
            .section_start(output_section_id::BSS, "__bss_start")
            .set_hidden(hidden);

        symbols
            .section_end(output_section_id::BSS, "end")
            .set_hidden(hidden);
        symbols
            .section_end(output_section_id::BSS, "_end")
            .set_hidden(hidden);
        symbols.section_end(output_section_id::BSS, "__end").hide();

        if args.arch == Architecture::RiscV64 {
            symbols.section_start(
                output_section_id::DATA,
                crate::elf::GLOBAL_POINTER_SYMBOL_NAME,
            );
        }

        if args.arch == Architecture::Ppc64 {
            symbols.section_start(output_section_id::GOT, crate::elf::TOC_SYMBOL_NAME);
        }

        symbols
            .section_end(output_section_id::DATA, "edata")
            .set_hidden(hidden);
        symbols
            .section_end(output_section_id::DATA, "_edata")
            .set_hidden(hidden);

        symbols
            .section_start(output_section_id::TDATA, "__tdata_start")
            .hide();

        if output_kind != OutputKind::StaticExecutable(RelocationModel::Fixed) {
            symbols.section_start(output_section_id::DYNAMIC, "_DYNAMIC");
        }

        symbols
            .add_symbol(InternalSymDefInfo::new(
                SymbolPlacement::LoadBaseAddress,
                b"__executable_start",
            ))
            .hide();

        // We define _TLS_MODULE_BASE_ either at the start or end of the TLS segment, depending on
        // whether we're building a shared object or an executable. This symbol is used for TLSDESC.
        // See https://www.fsfla.org/~lxoliva/writeups/TLS/RFC-TLSDESC-x86.txt for more details.
        let mut elf_symbol = SymtabEntry::<C>::default();
        elf_symbol.set_binding_and_type(object::elf::STB_GLOBAL, object::elf::STT_TLS);
        symbols.add_symbol(InternalSymDefInfo {
            placement: if output_kind == OutputKind::SharedObject {
                SymbolPlacement::SectionStart(output_section_id::TDATA)
            } else {
                SymbolPlacement::SectionEnd(output_section_id::TBSS)
            },
            name: b"_TLS_MODULE_BASE_",
            symbol: elf_symbol,
            is_provide: false,
        });
    }

    fn built_in_section_infos<'data>()
    -> Vec<crate::output_section_id::SectionOutputInfo<'data, Elf<C>>> {
        Self::SECTION_DEFINITIONS
            .iter()
            .map(|d| SectionOutputInfo {
                section_attributes: SectionAttributes {
                    flags: d.section_flags,
                    ty: d.ty,
                    entsize: d.element_size,
                    overrides: Default::default(),
                    received_input_flags: false,
                    class: PhantomData,
                },
                kind: d.kind,
                min_alignment: d.min_alignment,
                location_info: None,
                secondary_order: None,
                region_name: None,
                fill: None,
                phdrs: Vec::new(),
                input_order: false,
            })
            .collect()
    }

    fn create_finalise_sizes_ext<'data, 'states, 'files, A: Arch<Platform = Self>>(
        args: &ElfArgs,
        groups: &'files mut [layout::GroupState<'data, Self>],
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) -> Result<LayoutExt>
    where
        'data: 'files,
        'data: 'states,
    {
        LayoutExt::new::<C, A>(groups, args)
    }

    fn create_layout_ext<'data>(
        finalise_sizes_ext: Self::FinaliseSizesExt<'data>,
        _resolutions: &layout::SymbolResolutions<Self>,
    ) -> Result<Self::LayoutExt<'data>> {
        Ok(finalise_sizes_ext)
    }

    fn load_exception_frame_data<'data, 'scope, A: Arch<Platform = Self>>(
        object: &mut crate::layout::ObjectLayoutState<'data, Elf<C>>,
        common: &mut crate::layout::CommonGroupState<'data, Elf<C>>,
        eh_frame_section_index: object::SectionIndex,
        resources: &'scope crate::layout::GraphResources<'data, '_, Elf<C>>,
        queue: &mut crate::layout::LocalWorkQueue<Self>,
        scope: &rayon::Scope<'scope>,
    ) -> Result {
        object.format_specific.has_eh_frame_input = true;
        let eh_frame_section = object.object.section(eh_frame_section_index)?;
        let data = object.object.raw_section_data(eh_frame_section)?;
        let frame_index_offset = object.format_specific.exception_frames.len();
        let exception_frames = match object.relocations(eh_frame_section_index)? {
            RelocationList::Rela(relocations) => {
                ExceptionFrames::Rela(process_eh_frame_relocations::<C, A, ElfRela<C>>(
                    object,
                    common,
                    resources,
                    queue,
                    eh_frame_section,
                    eh_frame_section_index,
                    frame_index_offset,
                    data,
                    &RelaSequence(relocations),
                    scope,
                )?)
            }
            RelocationList::Crel(crel_iterator) => {
                ExceptionFrames::Crel(process_eh_frame_relocations::<C, A, ElfCrel<C>>(
                    object,
                    common,
                    resources,
                    queue,
                    eh_frame_section,
                    eh_frame_section_index,
                    frame_index_offset,
                    data,
                    &crel_iterator
                        .map(|raw| {
                            raw.map(|raw| ElfCrel {
                                raw,
                                class: PhantomData,
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    scope,
                )?)
            }
        };

        object
            .format_specific
            .exception_frames
            .extend(exception_frames);
        Ok(())
    }

    fn non_empty_section_loaded<'data, 'scope, A: Arch<Platform = Self>>(
        object: &mut layout::ObjectLayoutState<'data, Elf<C>>,
        common: &mut layout::CommonGroupState<'data, Elf<C>>,
        queue: &mut layout::LocalWorkQueue<Self>,
        unloaded: crate::resolution::UnloadedSection,
        resources: &'scope layout::GraphResources<'data, 'scope, Elf<C>>,
        scope: &Scope<'scope>,
    ) -> Result {
        let sizes = match &object.format_specific.exception_frames {
            ExceptionFrames::Rela(exception_frames) => {
                process_section_exception_frames::<C, A, ElfRela<C>>(
                    object,
                    unloaded.last_frame_index,
                    common,
                    resources,
                    queue,
                    scope,
                    exception_frames,
                )?
            }
            ExceptionFrames::Crel(exception_frames) => {
                process_section_exception_frames::<C, A, ElfCrel<C>>(
                    object,
                    unloaded.last_frame_index,
                    common,
                    resources,
                    queue,
                    scope,
                    exception_frames,
                )?
            }
        };

        object.format_specific.eh_frame_size += sizes.eh_frame_size;

        if resources.symbol_db.args.should_write_eh_frame_hdr {
            common.allocate(
                part_id::EH_FRAME_HDR,
                size_of::<EhFrameHdrEntry>() as u64 * sizes.num_frames,
            );
        }

        Ok(())
    }

    fn new_dynamic_layout_state_ext<'data>(
        _file: &Self::ResolvedDynamic<'data>,
        _args: &Self::Args,
    ) -> Self::DynamicLayoutStateExt<'data> {
        Default::default()
    }

    fn new_epilogue_layout<'data>(
        args: &ElfArgs,
        output_kind: OutputKind,
        dynamic_symbol_definitions: &mut [DynamicSymbolDefinition<'data, Self>],
        _group_states: &[layout::GroupState<'data, Self>],
    ) -> EpilogueLayoutExt {
        let gnu_hash_layout = create_gnu_hash_layout(args, output_kind, dynamic_symbol_definitions);

        let build_id_size = match &args.build_id {
            BuildIdOption::None => None,
            BuildIdOption::Fast => Some(size_of::<blake3::Hash>()),
            BuildIdOption::Hex(hex) => Some(hex.len()),
            BuildIdOption::Uuid => Some(size_of::<uuid::Uuid>()),
        };

        EpilogueLayoutExt {
            sysv_hash_layout: Default::default(),
            gnu_hash_layout,
            verdefs: Default::default(),
            build_id_size,
            needs_eh_frame_terminator: false,
        }
    }

    fn apply_non_addressable_indexes_epilogue(
        counts: &mut NonAddressableCounts,
        state: &mut EpilogueLayoutExt,
    ) {
        counts.verdef_count += state
            .verdefs
            .as_ref()
            .map(|v| v.len() as u16)
            .unwrap_or_default();
    }

    fn apply_non_addressable_indexes<'data, 'groups>(
        symbol_db: &SymbolDb<'data, Self>,
        counts: &NonAddressableCounts,
        mem_sizes_iter: impl Iterator<Item = &'groups mut OutputSectionPartMap<u64>>,
    ) {
        // If we were going to output symbol versions, but we didn't actually use any, then we drop
        // all versym allocations. This is partly to avoid wasting unnecessary space in the output
        // file, but mostly in order match what GNU ld does.
        if (counts.verneed_count == 0 && counts.verdef_count == 0)
            && symbol_db.output_kind.should_output_symbol_versions()
        {
            for mem_sizes in mem_sizes_iter {
                *mem_sizes.get_mut(part_id::GNU_VERSION) = 0;
            }
        }
    }

    fn finalise_sizes_epilogue<'data>(
        state: &mut EpilogueLayoutExt,
        mem_sizes: &mut OutputSectionPartMap<u64>,
        dynamic_symbol_definitions: &[DynamicSymbolDefinition<'data, Self>],
        properties: &LayoutExt,
        symbol_db: &SymbolDb<'data, Self>,
    ) {
        if symbol_db.output_kind.needs_dynamic() {
            let dynamic_entry_size = C::DYNAMIC_ENTRY_SIZE as usize;
            mem_sizes.increment(
                part_id::DYNAMIC,
                (elf_writer::NUM_EPILOGUE_DYNAMIC_ENTRIES * dynamic_entry_size) as u64,
            );
            if let Some(rpath) = symbol_db.args.rpath.as_ref() {
                mem_sizes.increment(part_id::DYNAMIC, dynamic_entry_size as u64);
                mem_sizes.increment(part_id::DYNSTR, rpath.len() as u64 + 1);
            }
            if let Some(soname) = symbol_db.args.soname.as_ref() {
                mem_sizes.increment(part_id::DYNSTR, soname.len() as u64 + 1);
                mem_sizes.increment(part_id::DYNAMIC, dynamic_entry_size as u64);
            }
            for aux in &symbol_db.args.auxiliary {
                mem_sizes.increment(part_id::DYNSTR, aux.len() as u64 + 1);
                mem_sizes.increment(part_id::DYNAMIC, dynamic_entry_size as u64);
            }

            mem_sizes.increment(
                part_id::DYNSTR,
                dynamic_symbol_definitions
                    .iter()
                    .map(|n| n.name.len() + 1)
                    .sum::<usize>() as u64,
            );
            mem_sizes.increment(
                part_id::DYNSYM,
                dynamic_symbol_definitions.len() as u64 * C::SYMTAB_ENTRY_SIZE,
            );
        }

        if let Some(build_id_sec_size) = state.gnu_build_id_note_section_size::<C>() {
            mem_sizes.increment(part_id::NOTE_GNU_BUILD_ID, build_id_sec_size);
        }

        mem_sizes.increment(
            part_id::NOTE_GNU_PROPERTY,
            gnu_property_notes_section_size::<C>(&properties.gnu_property_notes),
        );

        mem_sizes.increment(
            part_id::RISCV_ATTRIBUTES,
            properties.riscv_attributes.section_size,
        );

        if let Some(gnu_hash_layout) = state.gnu_hash_layout {
            gnu_hash_layout.allocate::<C>(mem_sizes);
        }

        let version_count = symbol_db.version_script.version_count();
        if version_count > 0 {
            // If soname is not provided, allocate space for file name as the base version
            let base_version_name = if symbol_db.args.soname.is_none() {
                let file_name = symbol_db
                    .args
                    .common
                    .output
                    .file_name()
                    .expect("File name should be present at this point")
                    .to_string_lossy()
                    .to_string();
                mem_sizes.increment(part_id::DYNSTR, file_name.len() as u64 + 1);
                file_name
            } else {
                String::new()
            };

            let mut verdefs = Vec::with_capacity(version_count.into());

            // Base version
            verdefs.push(VersionDef {
                name: base_version_name.into_bytes(),
                parent_index: None,
            });

            match &symbol_db.version_script {
                VersionScript::Regular(version_script) => {
                    // Take all but the base version
                    for version in version_script.version_iter().skip(1) {
                        verdefs.push(VersionDef {
                            name: version.name.to_vec(),
                            parent_index: version.parent_index,
                        });
                        mem_sizes.increment(part_id::DYNSTR, version.name.len() as u64 + 1);
                    }
                }
                VersionScript::Rust(_) => {}
            }

            let dependencies_count = symbol_db.version_script.parent_count();
            mem_sizes.increment(
                part_id::GNU_VERSION_D,
                (size_of::<crate::elf::Verdef>() as u16 * version_count
                    + size_of::<crate::elf::Verdaux>() as u16
                        * (version_count + dependencies_count))
                    .into(),
            );
            state.verdefs.replace(verdefs);
        }
    }

    fn finalise_layout_epilogue<'data>(
        epilogue_state: &mut EpilogueLayoutExt,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        symbol_db: &SymbolDb<'data, Self>,
        common_state: &LayoutExt,
        dynsym_start_index: u32,
        dynamic_symbol_defs: &[DynamicSymbolDefinition<Self>],
    ) -> Result {
        memory_offsets.increment(
            part_id::DYNSYM,
            dynamic_symbol_defs.len() as u64 * C::SYMTAB_ENTRY_SIZE,
        );

        if epilogue_state.needs_eh_frame_terminator {
            memory_offsets.increment(part_id::EH_FRAME, size_of::<u32>() as u64);
        }

        if let Some(build_id_sec_size) = epilogue_state.gnu_build_id_note_section_size::<C>() {
            memory_offsets.increment(part_id::NOTE_GNU_BUILD_ID, build_id_sec_size);
        }

        if let Some(gnu_hash_layout) = epilogue_state.gnu_hash_layout.as_mut() {
            gnu_hash_layout.symbol_base = dynsym_start_index;
        }

        memory_offsets.increment(
            part_id::NOTE_GNU_PROPERTY,
            crate::elf::gnu_property_notes_section_size::<C>(&common_state.gnu_property_notes),
        );

        memory_offsets.increment(
            part_id::RISCV_ATTRIBUTES,
            common_state.riscv_attributes.section_size,
        );

        if let Some(sysv_hash_layout) = epilogue_state.sysv_hash_layout.as_mut() {
            let additional = dynamic_symbol_defs.len() as u32;
            sysv_hash_layout.chain_count = dynsym_start_index
                .checked_add(additional)
                .context("Too many dynamic symbols for .hash")?;
        }

        if let Some(sysv_hash_layout) = &epilogue_state.sysv_hash_layout {
            memory_offsets.increment(part_id::SYSV_HASH, sysv_hash_layout.byte_size()?);
        }

        if let Some(verdefs) = &epilogue_state.verdefs {
            memory_offsets.increment(
                part_id::GNU_VERSION_D,
                (size_of::<crate::elf::Verdef>() * verdefs.len()
                    + size_of::<crate::elf::Verdaux>()
                        * (verdefs.len() + symbol_db.version_script.parent_count() as usize))
                    as u64,
            );
        }

        Ok(())
    }

    fn apply_late_size_adjustments_epilogue(
        state: &mut crate::elf::EpilogueLayoutExt,
        current_sizes: &OutputSectionPartMap<u64>,
        extra_sizes: &mut OutputSectionPartMap<u64>,
        dynamic_symbol_defs: &[DynamicSymbolDefinition<Self>],
        format_specific: &Self::FinaliseSizesExt<'_>,
        args: &ElfArgs,
    ) -> Result {
        if format_specific.has_eh_frame_input || current_sizes.get(part_id::EH_FRAME) != 0 {
            extra_sizes.increment(part_id::EH_FRAME, size_of::<u32>() as u64);
            state.needs_eh_frame_terminator = true;
        }

        if args.hash_style.includes_sysv() {
            allocate_sysv_hash(state, current_sizes, extra_sizes, dynamic_symbol_defs)?;
        }
        if args.is_relr_enabled() {
            let got_relr_size = current_sizes.get(part_id::GOT_RELR);
            let n = got_relr_size / C::GOT_ENTRY_SIZE;
            let relr_entries = got_relr_bitmap_relr_count::<C>(n);
            if relr_entries > 0 {
                extra_sizes.increment(part_id::RELR_DYN, relr_entries * C::RELR_ENTRY_SIZE);
            }
        }
        Ok(())
    }

    fn apply_late_size_adjustments_prelude(
        current_sizes: &OutputSectionPartMap<u64>,
        extra_sizes: &mut OutputSectionPartMap<u64>,
        format_specific: &LayoutExt,
        args: &ElfArgs,
    ) -> Result {
        extra_sizes.increment(
            part_id::GOT,
            C::GOT_ENTRY_SIZE
                * format_specific
                    .num_got_plt_header_entries(current_sizes.get(part_id::RELA_PLT) > 0),
        );

        if args.should_write_eh_frame_hdr && current_sizes.get(part_id::EH_FRAME_HDR) != 0 {
            extra_sizes.increment(part_id::EH_FRAME_HDR, size_of::<EhFrameHdr>() as u64);
        }
        Ok(())
    }

    fn finalise_sizes_all<'data>(
        mem_sizes: &mut OutputSectionPartMap<u64>,
        symbol_db: &SymbolDb<'data, Self>,
    ) {
        finalise_gnu_version_size(mem_sizes, symbol_db);
    }

    fn is_symbol_non_interposable<'data>(
        object: &Self::File<'data>,
        args: &Self::Args,
        sym: &Self::SymtabEntry,
        output_kind: OutputKind,
        export_list: Option<&crate::export_list::ExportList>,
        lib_name: &[u8],
        archive_semantics: bool,
        is_undefined: bool,
    ) -> bool {
        let symbol_is_exported = || {
            if let Some(export_list) = &export_list
                && let Ok(symbol_name) = object.symbol_name(sym)
                && !&export_list.contains(&UnversionedSymbolName::prehashed(symbol_name))
            {
                return false;
            }
            true
        };

        !sym.is_interposable()
            || sym.is_local()
            || output_kind.is_static_executable()
            // Symbols defined in an executable cannot be interposed since the executable is always the
            // first place checked for a symbol by the dynamic loader.
            || (!is_undefined && (
                output_kind.is_executable()
                || (archive_semantics && !args.should_export_dynamic(lib_name))
                || (
                    args.b_symbolic == BSymbolicKind::All
                    // `-Bsymbolic-functions`
                    || (
                        args.b_symbolic == BSymbolicKind::Functions
                        && sym.is_func()
                    )
                    // `-Bsymbolic-non-weak`
                    || (
                        args.b_symbolic == BSymbolicKind::NonWeak
                        && !sym.is_weak()
                    )
                    // `-Bsymbolic-non-weak-functions`
                    || (
                        args.b_symbolic == BSymbolicKind::NonWeakFunctions
                        && (sym.is_func()
                        && !sym.is_weak())
                    )
                )
                // Bsymbolic does not affect symbols that are exported
                && !(export_list.is_some() && symbol_is_exported())
            ))
    }

    fn validate_stack_section(
        input_section: &Self::SectionHeader,
        object: &impl std::fmt::Display,
        args: &Self::Args,
    ) -> Result {
        // If the .note.GNU-stack section has SHF_EXECINSTR, the input file is requesting an
        // executable stack.
        if input_section.is_executable() && !args.execstack {
            bail!("{object}: requires executable stack, but -z execstack is not specified");
        }
        Ok(())
    }

    fn finalise_sizes_for_symbol<'data>(
        common: &mut CommonGroupState<'data, Self>,
        symbol_db: &SymbolDb<'data, Self>,
        symbol_id: SymbolId,
        flags: ValueFlags,
    ) -> Result {
        if flags.is_dynamic() && flags.has_resolution() {
            let name = symbol_db.symbol_name(symbol_id)?;
            let name = Self::RawSymbolName::parse(name.bytes()).name();

            if flags.needs_copy_relocation() {
                // The dynamic symbol is a definition, so is handled by the epilogue. We only
                // need to deal with the symtab entry here.
                common.allocate(part_id::SYMTAB_GLOBAL, C::SYMTAB_ENTRY_SIZE);
                common.allocate(part_id::STRTAB, name.len() as u64 + 1);
                intern_strtab_name(&mut common.format_specific.strtab_names, name);
            } else if !flags.needs_canonical_plt() {
                common.allocate(part_id::DYNSTR, name.len() as u64 + 1);
                common.allocate(part_id::DYNSYM, C::SYMTAB_ENTRY_SIZE);
            }
        }

        if symbol_db.args.should_emit_got_plt_syms() && flags.needs_got() {
            let name = symbol_db.symbol_name(symbol_id)?;
            let name = Self::RawSymbolName::parse(name.bytes()).name();
            let name_len = name.len() + 4; // "$got" or "$plt" suffix

            let entry_size = C::SYMTAB_ENTRY_SIZE;
            common.allocate(part_id::SYMTAB_LOCAL, entry_size);
            common.allocate(part_id::STRTAB, name_len as u64 + 1);
            intern_strtab_name_with_suffix(&mut common.format_specific.strtab_names, name, b"$got");

            if flags.needs_plt() {
                common.allocate(part_id::SYMTAB_LOCAL, entry_size);
                common.allocate(part_id::STRTAB, name_len as u64 + 1);
                intern_strtab_name_with_suffix(
                    &mut common.format_specific.strtab_names,
                    name,
                    b"$plt",
                );
            }
        }

        Ok(())
    }

    fn allocate_resolution(
        flags: ValueFlags,
        mem_sizes: &mut OutputSectionPartMap<u64>,
        output_kind: OutputKind,
        args: &Self::Args,
    ) {
        let has_dynamic_symbol =
            flags.is_dynamic() || (flags.needs_export_dynamic() && flags.is_interposable());

        if flags.needs_got() && !flags.needs_tls_got() {
            let is_got_relr = is_got_relr_eligible(flags, has_dynamic_symbol, args, output_kind);
            if is_got_relr {
                mem_sizes.increment(part_id::GOT_RELR, C::GOT_ENTRY_SIZE);
            } else {
                mem_sizes.increment(part_id::GOT, C::GOT_ENTRY_SIZE);
            }
            if flags.needs_plt() {
                mem_sizes.increment(part_id::PLT_GOT, PLT_ENTRY_SIZE);
            }
            if flags.is_ifunc() || flags.needs_canonical_plt() {
                mem_sizes.increment(part_id::RELA_PLT, C::RELA_ENTRY_SIZE);
            } else if has_dynamic_symbol {
                mem_sizes.increment(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
            } else if flags.has_link_time_address() && output_kind.is_position_independent() {
                if args.is_relr_enabled() && !is_got_relr {
                    // Flat RELR for section boundary symbols (not bitmap-packed)
                    mem_sizes.increment(part_id::RELR_DYN, C::RELR_ENTRY_SIZE);
                } else if !args.is_relr_enabled() {
                    mem_sizes.increment(part_id::RELA_DYN_RELATIVE, C::RELA_ENTRY_SIZE);
                }
                // is_got_relr=true: RELR entries counted by post_compute_sizes
            }

            if flags.needs_canonical_plt_got_for_address() {
                mem_sizes.increment(part_id::GOT, C::GOT_ENTRY_SIZE);
                mem_sizes.increment(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
            }
        }

        if flags.needs_ifunc_got_for_address() {
            mem_sizes.increment(part_id::GOT, C::GOT_ENTRY_SIZE);
            if output_kind.is_position_independent() {
                if args.is_relr_enabled() {
                    mem_sizes.increment(part_id::RELR_DYN, C::RELR_ENTRY_SIZE);
                } else {
                    mem_sizes.increment(part_id::RELA_DYN_RELATIVE, C::RELA_ENTRY_SIZE);
                }
            }
        }

        if flags.needs_got_tls_offset() {
            mem_sizes.increment(part_id::GOT, C::GOT_ENTRY_SIZE);
            if flags.is_interposable() || output_kind.is_shared_object() {
                mem_sizes.increment(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
            }
        }

        if flags.needs_got_tls_module() {
            mem_sizes.increment(part_id::GOT, C::GOT_ENTRY_SIZE * 2);
            // For executables, the TLS module ID is known at link time. For shared objects, we need
            // a runtime relocation to fill it in.
            if !output_kind.is_executable() || flags.is_dynamic() {
                mem_sizes.increment(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
            }
            if has_dynamic_symbol {
                mem_sizes.increment(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
            }
        }

        if flags.needs_got_tls_descriptor() {
            mem_sizes.increment(part_id::GOT, C::GOT_ENTRY_SIZE * 2);
            mem_sizes.increment(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
        }
    }

    fn allocate_object_symtab_space<'data>(
        state: &ObjectLayoutState<'data, Elf<C>>,
        common: &mut CommonGroupState<'data, Elf<C>>,
        symbol_db: &SymbolDb<'data, Elf<C>>,
        per_symbol_flags: &AtomicPerSymbolFlags,
    ) -> Result {
        let mut num_locals = 0;
        let mut num_globals = 0;
        let mut strings_size = 0;
        for ((sym_index, sym), flags) in state
            .object
            .enumerate_symbols()
            .zip(per_symbol_flags.range(state.symbol_id_range()))
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
                // If we've decided to emit the symbol even though it's not referenced (because it's
                // in a section we're emitting), then make sure we have a resolution for it.
                flags.fetch_or(ValueFlags::DIRECT);
                if flags.get().is_symtab_local(sym) {
                    num_locals += 1;
                } else {
                    num_globals += 1;
                }
                let name = symtab_name_for_strtab(info.name);
                intern_strtab_name(&mut common.format_specific.strtab_names, name);
                strings_size += name.len() + 1;
            } else if symbol_db.args.should_output_partial_object
                && sym.is_undefined()
                && symbol_db.is_canonical(symbol_id)
                && let Ok(name) = state.object.symbol_name(sym)
                && !name.is_empty()
            {
                num_globals += 1;
                let name = symtab_name_for_strtab(name);
                intern_strtab_name(&mut common.format_specific.strtab_names, name);
                strings_size += name.len() + 1;
            }
        }
        let entry_size = C::SYMTAB_ENTRY_SIZE;
        common.allocate(part_id::SYMTAB_LOCAL, num_locals * entry_size);
        common.allocate(part_id::SYMTAB_GLOBAL, num_globals * entry_size);
        common.allocate(part_id::STRTAB, strings_size as u64);
        Ok(())
    }

    fn allocate_internal_symbol(
        symbol_id: SymbolId,
        def_info: &InternalSymDefInfo<Elf<C>>,
        sizes: &mut OutputSectionPartMap<u64>,
        symbol_db: &SymbolDb<Self>,
        format_specific: &mut CommonGroupStateExt,
    ) -> Result {
        // PROVIDE_HIDDEN symbols are local, others are global
        let symtab_part = if def_info.symbol.is_hidden() {
            part_id::SYMTAB_LOCAL
        } else {
            part_id::SYMTAB_GLOBAL
        };
        sizes.increment(symtab_part, C::SYMTAB_ENTRY_SIZE);
        let symbol_name = symbol_db.symbol_name(symbol_id)?;
        let symbol_name = symtab_name_for_strtab(symbol_name.bytes());
        intern_strtab_name(&mut format_specific.strtab_names, symbol_name);
        sizes.increment(part_id::STRTAB, symbol_name.len() as u64 + 1);

        Ok(())
    }

    fn allocate_thunk_symbol_sizes(
        sizes: &mut OutputSectionPartMap<u64>,
        symbols: &[SymbolId],
        symbol_db: &SymbolDb<Self>,
        format_specific: &mut CommonGroupStateExt,
    ) {
        let total_name_bytes: usize = symbols
            .iter()
            .map(|&sym_id| {
                let orig = symbol_db
                    .symbol_name(sym_id)
                    .map_or(&b""[..], |n| n.bytes());
                intern_strtab_name_with_suffix(
                    &mut format_specific.strtab_names,
                    THUNK_SYMBOL_PREFIX.as_bytes(),
                    orig,
                );
                THUNK_SYMBOL_PREFIX.len() + orig.len() + 1
            })
            .sum();
        sizes.increment(
            part_id::SYMTAB_LOCAL,
            symbols.len() as u64 * C::SYMTAB_ENTRY_SIZE,
        );
        sizes.increment(part_id::STRTAB, total_name_bytes as u64);
    }

    fn share_strtab_suffixes<'data>(
        group_states: &mut [layout::GroupState<'data, Self>],
        total_sizes: &mut OutputSectionPartMap<u64>,
        format_specific: &mut LayoutExt,
    ) {
        crate::timing_phase!("Share .strtab suffixes");
        let mut names = Vec::new();
        let mut unmerged = 0;
        for group in group_states.iter_mut() {
            names.append(&mut group.common.format_specific.strtab_names);
            unmerged += group.common.mem_sizes.get(part_id::STRTAB);
        }
        if unmerged == 0 {
            return;
        }

        let mut strtab = finalize_strtab(names);
        if strtab.bytes.is_empty() {
            strtab.bytes.push(0);
        }
        let merged = strtab.bytes.len() as u64;
        format_specific.strtab = strtab;

        for group in group_states.iter_mut() {
            let size = group.common.mem_sizes.get(part_id::STRTAB);
            if size > 0 {
                group.common.mem_sizes.decrement(part_id::STRTAB, size);
            }
        }
        total_sizes.decrement(part_id::STRTAB, unmerged);
        group_states[0]
            .common
            .mem_sizes
            .increment(part_id::STRTAB, merged);
        total_sizes.increment(part_id::STRTAB, merged);
    }

    fn allocate_prelude(common: &mut CommonGroupState<Self>, symbol_db: &SymbolDb<Self>) {
        // The first entry in the symbol table must be null. Similarly, the first string in the
        // strings table must be empty.
        if !symbol_db.args.should_strip_all() {
            common.allocate(part_id::SYMTAB_LOCAL, C::SYMTAB_ENTRY_SIZE);
            common.allocate(part_id::STRTAB, 1);
        }

        if symbol_db.output_kind.needs_dynsym() {
            // Allocate space for the null symbol.
            common.allocate(part_id::DYNSTR, 1);
            common.allocate(part_id::DYNSYM, C::SYMTAB_ENTRY_SIZE);
        }
    }

    fn finalise_prelude_layout<'data>(
        prelude: &layout::PreludeLayoutState<Self>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resources: &layout::FinaliseLayoutResources<'_, 'data, Elf<C>>,
    ) -> Result<Self::PreludeLayoutExt> {
        // Take the null symbol's index.
        if resources.symbol_db.output_kind.needs_dynsym() {
            Self::take_dynsym_index(memory_offsets, resources.section_layouts)?;
        }

        let got_plt_header_entries = resources.format_specific.num_got_plt_header_entries(
            resources
                .section_layouts
                .get(output_section_id::RELA_PLT)
                .mem_size
                > 0,
        );
        memory_offsets.increment(part_id::GOT, C::GOT_ENTRY_SIZE * got_plt_header_entries);

        let tlsld_got_entry = prelude.format_specific.needs_tlsld_got_entry.then(|| {
            let address = NonZeroU64::new(memory_offsets.get(part_id::GOT))
                .expect("GOT address must never be zero");
            memory_offsets.increment(part_id::GOT, C::GOT_ENTRY_SIZE * 2);
            address
        });

        Ok(PreludeLayoutExt {
            got_plt_header_entries,
            tlsld_got_entry,
        })
    }

    #[inline(always)]
    fn create_resolution(
        flags: ValueFlags,
        raw_value: u64,
        dynamic_symbol_index: Option<NonZeroU32>,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        args: &<Elf<C> as Platform>::Args,
        output_kind: OutputKind,
    ) -> Resolution<Elf<C>> {
        let mut resolution: Resolution<Elf<C>> = Resolution {
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
            resolution.format_specific.plt_address = Some(plt_address);
            if flags.is_dynamic() {
                resolution.raw_value = plt_address.get();
            }
            // For functions with address equality needs, allocate 2 GOT entries
            // - First entry: Used by PLT
            // - Second entry: Used by GOT-relative references
            let num_got_entries = if flags.needs_ifunc_got_for_address()
                || flags.needs_canonical_plt_got_for_address()
            {
                2
            } else {
                1
            };
            let has_dynamic_symbol =
                flags.is_dynamic() || (flags.needs_export_dynamic() && flags.is_interposable());
            let is_got_relr = is_got_relr_eligible(flags, has_dynamic_symbol, args, output_kind);
            resolution.format_specific.got_address = if is_got_relr && num_got_entries == 1 {
                Some(allocate_got_relr::<C>(memory_offsets))
            } else {
                Some(allocate_got::<C>(num_got_entries, memory_offsets))
            };
        } else if flags.needs_tls_got() {
            // Handle the TLS GOT addresses where we can combine up to 3 different access methods.
            let mut num_got_slots = 0;
            if flags.needs_got_tls_offset() {
                num_got_slots += 1;
            }
            if flags.needs_got_tls_module() {
                num_got_slots += 2;
            }
            if flags.needs_got_tls_descriptor() {
                num_got_slots += 2;
            }
            debug_assert!(num_got_slots > 0);
            resolution.format_specific.got_address =
                Some(allocate_got::<C>(num_got_slots, memory_offsets));
        } else if flags.needs_got() {
            let has_dynamic_symbol =
                flags.is_dynamic() || (flags.needs_export_dynamic() && flags.is_interposable());
            let is_got_relr = is_got_relr_eligible(flags, has_dynamic_symbol, args, output_kind);
            resolution.format_specific.got_address = if is_got_relr {
                Some(allocate_got_relr::<C>(memory_offsets))
            } else {
                Some(allocate_got::<C>(1, memory_offsets))
            };
        }

        resolution
    }

    fn validate_resolution(
        name: &[u8],
        resolution: &crate::layout::Resolution<Elf<C>>,
        got: &SectionHeader<C>,
        got_data: &[u8],
    ) -> Result {
        let flags = resolution.flags;
        if flags.is_ifunc()
            || flags.needs_got_tls_module()
            || flags.needs_got_tls_offset()
            || flags.needs_got_tls_descriptor()
        {
            return Ok(());
        }
        if let Some(got_address) = resolution.format_specific.got_address {
            let start_offset =
                (got_address.get() - Into::<u64>::into(got.sh_addr(LittleEndian))) as usize;
            let end_offset = start_offset + C::GOT_ENTRY_SIZE as usize;
            if end_offset > got_data.len() {
                bail!("GOT offset beyond end of GOT 0x{end_offset}");
            }
            if resolution.flags.is_dynamic() || resolution.flags.is_ifunc() {
                return Ok(());
            }
            let expected = resolution.raw_value;
            let address = Word::<C>::from_le_bytes(&got_data[start_offset..end_offset]).into();
            if expected != address {
                let name = String::from_utf8_lossy(name);
                bail!(
                    "flags={flags:?} `{name}` has address 0x{expected:x}, but GOT \
                 (at 0x{got_address:x}) points to 0x{address:x}"
                );
            }
        }
        Ok(())
    }

    fn raw_symbol_name<'data>(
        name_bytes: &'data [u8],
        verneed_table: &Self::VerneedTable<'data>,
        symbol_index: object::SymbolIndex,
    ) -> Self::RawSymbolName<'data> {
        if let Some(version_name) = verneed_table.version_name(symbol_index) {
            RawSymbolName {
                name: name_bytes,
                version_name: Some(version_name),
                is_default: false,
            }
        } else {
            RawSymbolName::parse(name_bytes)
        }
    }

    fn default_layout_rules(args: &Self::Args) -> Vec<SectionRule<'static>> {
        let sframe_outcome = if args.discard_sframe {
            SectionRuleOutcome::Discard
        } else {
            SectionRuleOutcome::Section(crate::layout_rules::SectionOutputInfo::keep(
                output_section_id::SFRAME,
            ))
        };

        let mut rules = Vec::with_capacity(
            DEFAULT_SECTION_PLACEMENT_RULES.len() + LINKER_MANAGED_SECTION_RULES.len() + 1,
        );

        if args.gdb_index {
            rules.push(SectionRule::exact(
                secnames::DEBUG_GNU_PUBNAMES,
                SectionRuleOutcome::DebugIndex,
            ));
            rules.push(SectionRule::exact(
                secnames::DEBUG_GNU_PUBTYPES,
                SectionRuleOutcome::DebugIndex,
            ));
        }

        rules.extend(DEFAULT_SECTION_PLACEMENT_RULES.iter().cloned());
        rules.extend(LINKER_MANAGED_SECTION_RULES.iter().cloned());

        rules.push(SectionRule::exact(
            secnames::SFRAME_SECTION_NAME,
            sframe_outcome,
        ));

        rules
    }

    fn linker_script_rules_pre_build(rule_builder: &mut crate::layout_rules::LayoutRulesBuilder) {
        // Even when we have a linker script, we still need to map .comment to .comment. It's a
        // special section because both input objects and the linker write to it. At least for
        // linkers that put their version in the .comment section. GNU ld doesn't, but LLD does and
        // still does so even when a linker script supposedly suppresses built-in rules.
        rule_builder.add_section_rule(SectionRule::exact_section_keep(
            secnames::COMMENT_SECTION_NAME,
            output_section_id::COMMENT,
        ));
        for rule in LINKER_MANAGED_SECTION_RULES {
            rule_builder.add_section_rule(rule.clone());
        }
    }

    fn init_section_priority(name: &[u8]) -> Option<u16> {
        init_fini_priority(name)
    }

    fn verify_allowed_input_section_name(name: &[u8]) -> Result {
        if name.starts_with(secnames::GNU_LTO_SYMTAB_PREFIX.as_bytes()) {
            if cfg!(all(feature = "plugins", unix)) {
                bail!("Found GCC LTO input that we didn't supply to linker plugin");
            }
            return Err(crate::symbol_db::linker_plugin_disabled_error());
        }

        Ok(())
    }

    fn allocate_header_sizes<'data>(
        prelude: &mut layout::PreludeLayoutState<'data, Self>,
        sizes: &mut OutputSectionPartMap<u64>,
        header_info: &layout::HeaderInfo,
        _program_segments: &ProgramSegments<Self::ProgramSegmentDef>,
        output_sections: &OutputSections<Self>,
        _resources: &layout::FinaliseSizesResources<'data, '_, Self>,
        _args: &Self::Args,
    ) {
        sizes.increment(crate::part_id::FILE_HEADER, u64::from(C::FILE_HEADER_SIZE));
        sizes.increment(
            part_id::PROGRAM_HEADERS,
            program_headers_size::<C>(header_info),
        );
        sizes.increment(
            part_id::SECTION_HEADERS,
            section_headers_size::<C>(header_info),
        );
        prelude.format_specific.shstrtab_size = crate::elf::shstrtab_from_sections(output_sections)
            .bytes
            .len() as u64;
        sizes.increment(part_id::SHSTRTAB, prelude.format_specific.shstrtab_size);
    }

    fn copy_relocate_symbol<'scope, 'data>(
        state: &mut layout::DynamicLayoutState<Elf<C>>,
        symbol_id: SymbolId,
        resources: &layout::GraphResources<'data, 'scope, Elf<C>>,
    ) -> Result {
        let symbol = state
            .object
            .symbol(state.symbol_id_range().id_to_input(symbol_id))?;

        // Note, we're a shared object, so this is the address relative to the load address of the
        // shared object, not an offset within a section like with regular input objects. That means
        // that we don't need to take the section into account.
        let address = symbol.value();

        let info = state
            .format_specific
            .copy_relocations
            .entry(address)
            .or_insert_with(|| CopyRelocationInfo {
                symbol_id,
                is_weak: symbol.is_weak(),
            });

        info.add_symbol(symbol_id, symbol.is_weak(), resources);

        Ok(())
    }

    fn finalise_copy_relocations<'data>(
        group_states: &mut [layout::GroupState<'data, Self>],
        symbol_db: &SymbolDb<'data, Self>,
        symbol_flags: &AtomicPerSymbolFlags,
    ) -> Result {
        finalise_copy_relocations(group_states, symbol_db, symbol_flags)
    }

    fn take_dynsym_index(
        memory_offsets: &mut OutputSectionPartMap<u64>,
        section_layouts: &OutputSectionMap<OutputRecordLayout>,
    ) -> Result<u32> {
        let index = u32::try_from(
            (memory_offsets.get(part_id::DYNSYM)
                - section_layouts.get(output_section_id::DYNSYM).mem_offset)
                / C::SYMTAB_ENTRY_SIZE,
        )
        .context("Too many dynamic symbols")?;
        memory_offsets.increment(part_id::DYNSYM, C::SYMTAB_ENTRY_SIZE);
        Ok(index)
    }

    fn build_output_order_and_program_segments<'data>(
        custom: &CustomSectionIds,
        output_kind: OutputKind,
        output_sections: &OutputSections<'data, Self>,
        secondary: &OutputSectionMap<Vec<OutputSectionId>>,
        location_counters: &[crate::layout_rules::LocationCounter<'data>],
    ) -> (OutputOrder<'data>, ProgramSegments<Self::ProgramSegmentDef>) {
        let mut builder = OutputOrderBuilder::<Self>::new(
            Self::program_segment_defs().to_vec(),
            output_kind,
            output_sections,
            secondary,
            false,
            location_counters,
        );

        builder.add_section(crate::output_section_id::FILE_HEADER);
        builder.add_section(output_section_id::PROGRAM_HEADERS);
        builder.add_section(output_section_id::SECTION_HEADERS);
        builder.add_section(output_section_id::NOTE_GNU_PROPERTY);
        builder.add_section(output_section_id::NOTE_GNU_BUILD_ID);
        builder.add_section(output_section_id::INTERP);
        builder.add_section(output_section_id::NOTE_ABI_TAG);
        builder.add_section(output_section_id::HASH);
        builder.add_section(output_section_id::GNU_HASH);
        builder.add_section(output_section_id::DYNSYM);
        builder.add_section(output_section_id::DYNSTR);
        builder.add_section(output_section_id::GNU_VERSION);
        builder.add_section(output_section_id::GNU_VERSION_D);
        builder.add_section(output_section_id::GNU_VERSION_R);
        builder.add_section(output_section_id::RELA_DYN_RELATIVE);
        builder.add_section(output_section_id::RELR_DYN);
        builder.add_section(output_section_id::RELA_PLT);
        builder.add_section(output_section_id::RODATA);
        builder.add_section(output_section_id::EH_FRAME_HDR);
        builder.add_section(output_section_id::EH_FRAME);
        builder.add_section(output_section_id::SFRAME);
        builder.add_section(output_section_id::GCC_EXCEPT_TABLE);
        builder.add_sections(&custom.ro);

        builder.add_section(output_section_id::PLT_GOT);
        builder.add_section(output_section_id::INIT);
        builder.add_section(output_section_id::FINI);
        if custom.place_after_similar {
            builder.add_section(output_section_id::TEXT);
            builder.add_sections(&custom.exec);
        } else {
            builder.add_sections(&custom.exec);
            // Thunk generation only supports emitting thunks before the primary
            // function part, so unnamed exec sections stay before `.text` when
            // there is no linker script.
            builder.add_section(output_section_id::TEXT);
        }

        builder.add_section(output_section_id::TDATA);
        builder.add_sections(&custom.tdata);
        builder.add_section(output_section_id::TBSS);
        builder.add_sections(&custom.tbss);
        builder.add_section(output_section_id::INIT_ARRAY);
        builder.add_section(output_section_id::FINI_ARRAY);
        builder.add_section(output_section_id::PREINIT_ARRAY);
        builder.add_section(output_section_id::DATA_REL_RO);
        builder.add_section(output_section_id::DYNAMIC);
        builder.add_section(output_section_id::GOT);
        builder.add_section(output_section_id::RELRO_PADDING);
        builder.add_section(output_section_id::DATA);
        builder.add_sections(&custom.data);
        builder.add_section(output_section_id::BSS);
        builder.add_sections(&custom.bss);

        builder.add_sections(&custom.nonalloc);
        builder.add_section(output_section_id::GDB_INDEX);
        builder.add_section(output_section_id::COMMENT);
        builder.add_section(output_section_id::RISCV_ATTRIBUTES);
        builder.add_section(output_section_id::SHSTRTAB);
        builder.add_section(output_section_id::SYMTAB_LOCAL);
        builder.add_section(output_section_id::SYMTAB_SHNDX_LOCAL);
        builder.add_section(output_section_id::STRTAB);

        builder.build()
    }

    fn build_custom_output_order_and_program_segments<'data>(
        custom: &CustomSectionIds,
        output_kind: OutputKind,
        output_sections: &OutputSections<'data, Self>,
        secondary: &OutputSectionMap<Vec<OutputSectionId>>,
        linker_scripts: &[&SequencedLinkerScript<'data, Self>],
        location_counters: &[crate::layout_rules::LocationCounter<'data>],
    ) -> Result<(OutputOrder<'data>, ProgramSegments<Self::ProgramSegmentDef>)> {
        let mut builder = OutputOrderBuilder::<Self>::new(
            Self::program_segment_defs().to_vec(),
            output_kind,
            output_sections,
            secondary,
            true,
            location_counters,
        );

        let mut segments_map = HashMap::new();
        let mut ordered_sections = Vec::new();
        let mut segment_entries = Vec::new();

        let mut num_phdrs = 0;
        let mut has_filehdr = false;
        let mut load_without_hdrs = false;

        for script in linker_scripts {
            num_phdrs += script.parsed.program_headers.len();
            for phdr in &script.parsed.program_headers {
                let ptype = expression_eval::evaluate_const(&phdr.ptype)? as u32;
                if ptype == pt::LOAD.0 {
                    if phdr.has_filehdr || phdr.has_phdrs {
                        if load_without_hdrs {
                            bail!(
                                "PHDRS and FILEHDR are not supported when prior PT_LOAD headers lack them"
                            );
                        }
                    } else {
                        load_without_hdrs = true;
                    }
                }
                let flags = phdr
                    .flags
                    .as_ref()
                    .map(|f| expression_eval::evaluate_const(f).map(|c| c as u32))
                    .transpose()?
                    .unwrap_or({
                        if phdr.has_filehdr || phdr.has_phdrs {
                            pf::READABLE.0
                        } else {
                            0
                        }
                    });

                let id = builder.add_custom_segment(
                    <ProgramSegmentDef as platform::ProgramSegmentDef>::from_linker_script(
                        ptype, flags,
                    ),
                );
                has_filehdr = has_filehdr || phdr.has_filehdr;
                let at_lma = phdr
                    .at_address
                    .as_ref()
                    .map(expression_eval::evaluate_const)
                    .transpose()?;
                segment_entries.push(SegmentEntry {
                    id,
                    ptype,
                    flags,
                    is_emitted: phdr.has_filehdr || phdr.has_phdrs,
                    has_explicit_flags: phdr.flags.is_some(),
                    has_filehdr: phdr.has_filehdr,
                    has_phdrs: phdr.has_phdrs,
                    at_lma,
                });
                segments_map.insert(phdr.name, id);
            }

            for (index, id) in script.parsed.ordered_sections.iter().enumerate() {
                if !output_sections.should_emit_only_if_order_slot(*id, index) {
                    continue;
                }
                let info = output_sections.section_infos.get(*id);
                for phdr in &info.phdrs {
                    if phdr == b"NONE" {
                        continue;
                    }
                    let segment = segments_map.get(phdr).with_context(|| {
                        format!(
                            "Section {} assigned to non-existent phdr `{}`",
                            output_sections.display_name(*id),
                            String::from_utf8_lossy(phdr)
                        )
                    })?;
                    let entry = &mut segment_entries[segment.as_usize()];
                    entry.is_emitted = true;
                    if !matches!(SegmentType(entry.ptype), pt::LOAD | pt::PHDR)
                        && (entry.has_filehdr || entry.has_phdrs)
                    {
                        bail!(
                            "Non-load segment {} includes file header and/or program header",
                            segment.as_usize()
                        );
                    }
                    if entry.has_explicit_flags {
                        continue;
                    }
                    entry.flags |=
                        Self::get_segment_flags_for_section(&info.section_attributes.flags);
                    builder.get_segment_mut(*segment).segment_flags |= SegmentFlags(entry.flags);
                }
                ordered_sections.push(*id);
            }
        }

        let mut first_load = None;
        let mut starts = vec![None; num_phdrs];
        let mut ends = vec![None; num_phdrs];
        let mut first_exec: Option<ProgramSegmentId> = None;
        let mut first_rw: Option<ProgramSegmentId> = None;
        let mut first_ro: Option<ProgramSegmentId> = None;

        for (pos, id) in ordered_sections.iter().enumerate() {
            let phdrs = &output_sections.section_infos.get(*id).phdrs;
            for &phdr_name in phdrs {
                let Some(id) = segments_map.get(phdr_name) else {
                    continue;
                };
                let seg_idx = id.as_usize();
                let entry = segment_entries[seg_idx];
                if entry.ptype == pt::LOAD.0 && first_load.is_none() {
                    first_load = Some(*id);
                }
                if starts[seg_idx].is_none() {
                    starts[seg_idx] = Some(pos);
                }
                ends[seg_idx] = Some(pos);
                if entry.ptype == pt::LOAD.0 {
                    if (entry.flags & pf::EXECUTABLE.0) != 0
                        && first_exec.is_none_or(|e| {
                            segment_entries[e.as_usize()].flags & pf::WRITABLE.0 != 0
                        })
                    {
                        first_exec = Some(*id);
                    } else if (entry.flags & pf::WRITABLE.0) != 0
                        && first_rw.is_none_or(|e| {
                            segment_entries[e.as_usize()].flags & pf::EXECUTABLE.0 != 0
                        })
                    {
                        first_rw = Some(*id);
                    } else if first_ro
                        .is_none_or(|e| segment_entries[e.as_usize()].flags != pf::READABLE.0)
                    {
                        first_ro = Some(*id);
                    }
                }
            }
        }

        if first_load.is_none() {
            bail!("Missing LOAD PHDR in linker script");
        }

        let update_flags = |builder: &mut OutputOrderBuilder<Self>,
                            sections: &[OutputSectionId],
                            segment: ProgramSegmentId| {
            if segment_entries[segment.as_usize()].has_explicit_flags {
                return;
            }
            for &section_id in sections {
                let info = output_sections.section_infos.get(section_id);
                let flags = Self::get_segment_flags_for_section(&info.section_attributes.flags);
                builder.get_segment_mut(segment).segment_flags |= SegmentFlags(flags);
            }
        };

        let mut pending = custom.clone();

        for (pos, section_id) in ordered_sections.iter().enumerate() {
            let section_id = *section_id;

            if pos == 0 {
                let mut header_events = Vec::new();

                for (seg_idx, segment) in starts.iter().enumerate().take(num_phdrs) {
                    let entry = segment_entries[seg_idx];
                    let seg_id = ProgramSegmentId::new(seg_idx);
                    if entry.has_filehdr {
                        header_events.push((0, seg_id));
                        if ends[seg_idx].is_none() && !entry.has_phdrs {
                            header_events.push((1, seg_id));
                        }
                    } else if entry.has_phdrs {
                        header_events.push((2, seg_id));
                    } else if *segment == Some(pos) {
                        header_events.push((4, seg_id));
                    }
                    if ends[seg_idx].is_none() && entry.has_phdrs {
                        header_events.push((3, seg_id));
                    }
                }

                header_events.sort_by_key(|&(cat, _)| cat);
                let mut it = header_events.into_iter().peekable();

                while let Some((_, seg_id)) = it.next_if(|&(cat, _)| cat == 0) {
                    builder.push_event(OrderEvent::SegmentStart(seg_id));
                }
                builder.push_event(OrderEvent::Section(crate::output_section_id::FILE_HEADER));
                while let Some((_, seg_id)) = it.next_if(|&(cat, _)| cat == 1) {
                    builder.push_event(OrderEvent::SegmentEnd(seg_id));
                }
                while let Some((_, seg_id)) = it.next_if(|&(cat, _)| cat == 2) {
                    builder.push_event(OrderEvent::SegmentStart(seg_id));
                }
                builder.push_event(OrderEvent::Section(output_section_id::PROGRAM_HEADERS));
                while let Some((_, seg_id)) = it.next_if(|&(cat, _)| cat == 3) {
                    builder.push_event(OrderEvent::SegmentEnd(seg_id));
                }
                builder.push_event(OrderEvent::Section(output_section_id::SECTION_HEADERS));
                for (_, seg_id) in it {
                    builder.queue_segment_start(seg_id);
                }
            } else {
                for (seg_idx, segment) in starts.iter().enumerate().take(num_phdrs) {
                    let entry = segment_entries[seg_idx];
                    if *segment == Some(pos) && !entry.has_filehdr && !entry.has_phdrs {
                        builder.queue_segment_start(ProgramSegmentId::new(seg_idx));
                    }
                }
            }

            builder.add_section(section_id);

            let this_attr = &output_sections
                .section_infos
                .get(section_id)
                .section_attributes;
            let this_class = CustomSectionIds::class_of::<Self>(this_attr);
            let next_class = ordered_sections.get(pos + 1).map(|next| {
                CustomSectionIds::class_of::<Self>(
                    &output_sections.section_infos.get(*next).section_attributes,
                )
            });
            if this_class != crate::output_section_id::OrphanClass::NonAlloc
                && next_class != Some(this_class)
            {
                let orphans = pending.take_class(this_class);
                if !orphans.is_empty() {
                    for phdr in &output_sections.section_infos.get(section_id).phdrs {
                        if let Some(&seg_id) = segments_map.get(phdr) {
                            update_flags(&mut builder, &orphans, seg_id);
                        }
                    }
                    builder.add_sections(&orphans);
                }
            }

            for (seg_idx, segment) in ends.iter().enumerate().take(num_phdrs) {
                if *segment == Some(pos) {
                    let seg_id = ProgramSegmentId::new(seg_idx);
                    if Some(seg_id) == first_exec.or(first_load) {
                        update_flags(&mut builder, &pending.exec, seg_id);
                        builder.add_sections(&pending.exec);
                        pending.exec.clear();
                    }
                    if Some(seg_id) == first_rw.or(first_load) {
                        update_flags(&mut builder, &pending.tdata, seg_id);
                        update_flags(&mut builder, &pending.tbss, seg_id);
                        update_flags(&mut builder, &pending.data, seg_id);
                        update_flags(&mut builder, &pending.bss, seg_id);
                        builder.add_sections(&pending.tdata);
                        builder.add_sections(&pending.tbss);
                        builder.add_sections(&pending.data);
                        builder.add_sections(&pending.bss);
                        pending.tdata.clear();
                        pending.tbss.clear();
                        pending.data.clear();
                        pending.bss.clear();
                    }
                    if Some(seg_id) == first_ro.or(first_load) {
                        update_flags(&mut builder, &pending.ro, seg_id);
                        builder.add_sections(&pending.ro);
                        pending.ro.clear();
                    }
                    builder.push_event(OrderEvent::SegmentEnd(seg_id));
                }
            }
        }

        for &id in &pending.nonalloc {
            if id != output_section_id::RISCV_ATTRIBUTES {
                builder.add_section(id);
            }
        }

        for segment in &segment_entries {
            if !segment.is_emitted {
                builder.push_event(OrderEvent::SegmentStart(segment.id));
                builder.push_event(OrderEvent::SegmentEnd(segment.id));
            }
        }

        let riscv_segment = builder.add_custom_segment(
            <ProgramSegmentDef as platform::ProgramSegmentDef>::from_linker_script(
                pt::RISCV_ATTRIBUTES.0,
                pf::READABLE.0,
            ),
        );
        builder.push_event(OrderEvent::SegmentStart(riscv_segment));
        builder.add_section(output_section_id::RISCV_ATTRIBUTES);
        builder.push_event(OrderEvent::SegmentEnd(riscv_segment));

        let (order, mut program_segments) = builder.build();
        for entry in &segment_entries {
            if let Some(at_lma) = entry.at_lma {
                program_segments.set_at_lma(entry.id, at_lma);
            }
        }
        Ok((order, program_segments))
    }

    fn will_emit_section_symbol_for_partial_objects(
        output_sections: &OutputSections<Self>,
        section_id: OutputSectionId,
    ) -> bool {
        if !output_sections.will_emit_section(section_id) {
            return false;
        }

        if matches!(
            section_id,
            crate::output_section_id::FILE_HEADER
                | output_section_id::PROGRAM_HEADERS
                | output_section_id::SECTION_HEADERS
        ) {
            return false;
        }

        let section_attr = output_sections.output_info(section_id).section_attributes;
        let segment_type = section_id
            .opt_built_in_details::<Elf<C>>()
            .and_then(|d| d.target_segment_type)
            .unwrap_or(linker_utils::elf::pt::LOAD);
        if section_attr.is_null() {
            false
        } else {
            let type_id = section_attr.ty();
            !type_id.is_rela()
                && !type_id.is_rel()
                && !type_id.is_symtab()
                && !type_id.is_strtab()
                && segment_type == linker_utils::elf::pt::LOAD
        }
    }

    fn lookup_for_partial_link(
        section_name: &[u8],
        section: &Self::SectionHeader,
        args: &Self::Args,
    ) -> SectionRuleOutcome {
        if section.should_exclude() {
            return SectionRuleOutcome::Discard;
        }

        if section_name.is_empty() {
            return crate::layout_rules::unnamed_section_output::<Elf<C>>(section);
        }

        match section_name {
            secnames::STRTAB_SECTION_NAME
            | secnames::SYMTAB_SECTION_NAME
            | secnames::SHSTRTAB_SECTION_NAME
            | secnames::SYMTAB_SHNDX_SECTION_NAME
            | secnames::GROUP_SECTION_NAME => {
                return SectionRuleOutcome::Discard;
            }
            secnames::RISCV_ATTRIBUTES_SECTION_NAME => return SectionRuleOutcome::RiscVAttribute,
            secnames::NOTE_GNU_PROPERTY_SECTION_NAME => return SectionRuleOutcome::NoteGnuProperty,
            secnames::NOTE_ABI_TAG_SECTION_NAME => {
                return SectionRuleOutcome::Section(crate::layout_rules::SectionOutputInfo::keep(
                    output_section_id::NOTE_ABI_TAG,
                ));
            }
            secnames::DEBUG_GNU_PUBNAMES | secnames::DEBUG_GNU_PUBTYPES if args.gdb_index => {
                return SectionRuleOutcome::DebugIndex;
            }
            _ => {}
        }

        SectionRuleOutcome::Custom
    }

    fn requires_symtab_shndx(num_sections: usize) -> bool {
        num_sections >= object::elf::SHN_LORESERVE as usize
    }

    fn compute_symtab_shndx_section_size(
        group_sizes: &mut OutputSectionPartMap<u64>,
        total_sizes: &mut OutputSectionPartMap<u64>,
    ) {
        let locals = group_sizes.get(part_id::SYMTAB_LOCAL) / C::SYMTAB_ENTRY_SIZE;
        let globals = group_sizes.get(part_id::SYMTAB_GLOBAL) / C::SYMTAB_ENTRY_SIZE;

        let mut extra_sizes = group_sizes.new_empty_like();
        extra_sizes.increment(
            part_id::SYMTAB_SHNDX_LOCAL,
            locals * SYMTAB_SHNDX_ENTRY_SIZE,
        );
        extra_sizes.increment(
            part_id::SYMTAB_SHNDX_GLOBAL,
            globals * SYMTAB_SHNDX_ENTRY_SIZE,
        );

        group_sizes.merge(&extra_sizes);
        total_sizes.merge(&extra_sizes);
    }

    type GdbIndexScanResult<'data> = crate::gdb_index::GdbIndexScanResult<'data>;

    fn compute_gdb_index_size<'data>(
        groups: &[Self::GroupState<'data>],
    ) -> crate::error::Result<(u64, Option<Self::GdbIndexScanResult<'data>>)> {
        crate::gdb_index::compute_gdb_index_size(groups)
    }

    fn align_load_segment_start(
        _segment_def: Self::ProgramSegmentDef,
        segment_alignment: Alignment,
        file_offset: &mut usize,
        mem_offset: &mut u64,
    ) {
        *mem_offset = segment_alignment.align_modulo(*file_offset as u64, *mem_offset);
    }

    fn default_symtab_entry() -> Self::SymtabEntry {
        Default::default()
    }

    fn get_sizeof_headers(header_info: &layout::HeaderInfo) -> u64 {
        u64::from(C::FILE_HEADER_SIZE) + program_headers_size::<C>(header_info)
    }

    fn handle_debug_index_section<'data>(
        obj: &mut crate::resolution::ResolvedObject<'data, Self>,
        section_index: object::SectionIndex,
        input_section: &'data Self::SectionHeader,
        member: &bumpalo_herd::Member<'data>,
        loaded_metrics: &LoadedMetrics,
    ) -> Result {
        let data = obj
            .common
            .object
            .section_data(input_section, member, loaded_metrics)?;

        obj.format_specific
            .debug_index_sections
            .push(InputDebugIndexSection {
                contents: data,
                section_index,
            });

        Ok(())
    }

    fn new_object_layout_state_ext<'data>(
        input: Self::ResolvedObjectExt<'data>,
    ) -> Self::ObjectLayoutStateExt<'data> {
        ObjectLayoutStateExt {
            debug_index_sections: input.debug_index_sections,
            ..Default::default()
        }
    }

    fn is_allowed_in_archive(kind: crate::file_kind::FileKind) -> bool {
        kind == FileKind::ElfObject
    }

    fn version_script_version_count(symbol_db: &Self::SymbolDb<'_>) -> u16 {
        symbol_db.version_script.version_count()
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

    fn apply_linker_script_attributes(
        linker_script_attributes: &linker_script::SectionAttributes,
        mut output_attributes: Self::SectionAttributes,
    ) -> Self::SectionAttributes {
        match linker_script_attributes {
            linker_script::SectionAttributes::Noload => {
                output_attributes.ty = sht::NOBITS;
                output_attributes.overrides.has_fixed_type = true;
            }
            linker_script::SectionAttributes::Readonly => {
                output_attributes.overrides.avoid_progpogation = shf::WRITE;
            }
            linker_script::SectionAttributes::Dsect
            | linker_script::SectionAttributes::Copy
            | linker_script::SectionAttributes::Info
            | linker_script::SectionAttributes::Overlay => {
                output_attributes.overrides.avoid_progpogation = shf::ALLOC;
            }
            linker_script::SectionAttributes::Type(ty) => {
                apply_script_type(&mut output_attributes, *ty);
            }
            linker_script::SectionAttributes::ReadonlyType(ty) => {
                apply_script_type(&mut output_attributes, *ty);
                output_attributes.overrides.avoid_progpogation = shf::WRITE;
            }
        }
        output_attributes
    }
}

fn apply_script_type<C: ElfClass>(output_attributes: &mut SectionAttributes<C>, ty: u32) {
    output_attributes.ty = SectionType(ty);
    output_attributes.overrides.has_script_type = true;
    output_attributes.set_alloc();
    if output_attributes.ty == sht::INIT_ARRAY
        || output_attributes.ty == sht::FINI_ARRAY
        || output_attributes.ty == sht::PREINIT_ARRAY
    {
        output_attributes.entsize = C::ADDRESS_SIZE;
    }
}

/// Marks the symbol version associated with the dynamic symbol `GLIBC_ABI_DT_RELR` as needed.
/// Referencing the version will cause the binary to error if it's loaded with a glibc that doesn't
/// support relr. glibc will error at startup if we use relr and don't reference the version. If
/// we're not linking against glibc, then the symbol (and version) will be absent. This is not an
/// error and the binary will work fine provided the dynamic loader supports relr.
pub(super) fn load_glibc_abi_dt_relr_version<C: ElfClass>(
    groups: &mut [layout::GroupState<'_, Elf<C>>],
    symbol_db: &SymbolDb<Elf<C>>,
) -> Result {
    if let Some(symbol_id) =
        symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(b"GLIBC_ABI_DT_RELR"))
    {
        let file_id = symbol_db.file_id_for_symbol(symbol_id);
        if let layout::FileLayoutState::Dynamic(state) =
            &mut groups[file_id.group()].files[file_id.file()]
        {
            let symbol_index = state.symbol_id_range.id_to_offset(symbol_id);
            state
                .format_specific
                .mark_version_as_needed(state.object.versym[symbol_index])?;
        }
    }

    Ok(())
}
