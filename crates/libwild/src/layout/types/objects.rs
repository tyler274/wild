use super::*;
use crate::bail;
use crate::error::Context;
use crate::error::Error;
use crate::error::Result;
use crate::layout::EnginePlatform;
use crate::layout::graph::*;
use crate::layout::section_debug;
use crate::layout::sections::*;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::SectionHeader as _;
use crate::platform::Symbol as _;
use crate::resolution::ScriptSortedSectionDetail;
use crate::resolution::SectionSlot;
use crate::resolution::UnloadedSection;
use crate::string_merging::get_merged_string_output_address;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::ValueFlags;
use linker_utils::relaxation::opt_input_to_output;
use object::SectionIndex;
use rayon::Scope;
use smallvec::SmallVec;
use std::num::NonZeroU32;

impl<'data, P: EnginePlatform> ObjectLayoutState<'data, P> {
    #[inline(always)]
    pub(crate) fn activate<'scope, A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        resources: &'scope GraphResources<'data, 'scope, P>,
        queue: &mut LocalWorkQueue<P>,
        scope: &Scope<'scope>,
    ) -> Result {
        P::activate_object_gc::<A>(
            self,
            common,
            crate::layout::platform_graph(resources),
            queue,
            scope,
        )?;

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

impl<'data, P: EnginePlatform<GcUnit = SectionGcUnit>> ObjectLayoutState<'data, P> {
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
                SectionSlot::Unloaded(_) => {
                    if no_gc {
                        queue.send_gc_unit_request::<A>(
                            self.file_id,
                            SectionGcUnit::new(object::SectionIndex(i)),
                            resources,
                            scope,
                        );
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
                crate::layout::platform_graph(resources),
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

impl<'data, P: EnginePlatform> ObjectLayoutState<'data, P> {
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
            crate::layout::platform_graph(resources),
            section,
            section_index,
            scope,
        )?;

        tracing::debug!(loaded_section = %self.object.section_display_name(section_index), file = %self.input);

        self.sections[section_index.0] = if unloaded.needs_sorting {
            self.script_sorted_sections.push(ScriptSortedSectionDetail {
                index: section_index,
                sort_by_init_priority: unloaded.sort_by_init_priority,
                sort_by_alignment: unloaded.sort_by_alignment,
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
            P::non_empty_section_loaded::<A>(
                self,
                common,
                queue,
                unloaded,
                crate::layout::platform_graph(resources),
                scope,
            )?;
        } else if P::is_zero_sized_section_content(section_id) {
            resources.keep_section(section_id);
        }

        P::load_associated_reloc_sections::<A>(
            self,
            common,
            queue,
            crate::layout::platform_graph(resources),
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
    pub(crate) fn new<P: EnginePlatform>(
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

impl<'data, P: EnginePlatform> ObjectLayout<'data, P> {
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
