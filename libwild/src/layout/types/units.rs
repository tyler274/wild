use super::*;
use crate::OutputKind;
use crate::alignment;
use crate::bail;
use crate::error::Context;
use crate::error::Error;
use crate::error::Result;
use crate::grouping::Group;
use crate::input_data::PRELUDE_FILE_ID;
use crate::layout::graph::*;
use crate::layout::script::*;
use crate::layout::sizes::*;
use crate::linker_script::Expression;
use crate::output_section_id;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::SymbolPlacement;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::ProgramSegmentDef as _;
use crate::platform::SectionAttributes as _;
use crate::platform::Symbol as _;
use crate::program_segments::ProgramSegmentId;
use crate::program_segments::ProgramSegments;
use crate::resolution;
use crate::sharding::ShardKey;
use crate::string_merging::MergedStringsSection;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolIdRange;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::FlagsForSymbol as _;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use itertools::Itertools;
use rayon::Scope;
use std::ffi::CString;
use std::mem::replace;
use std::mem::size_of;

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
            common,
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
        common: &mut CommonGroupState<'data, P>,
        extra_sizes: &mut OutputSectionPartMap<u64>,
    ) -> Result<(), Error> {
        if symbol_db.args.should_strip_all() {
            return Ok(());
        }

        self.internal_symbols.allocate_symbol_table_sizes(
            extra_sizes,
            symbol_db,
            &mut common.format_specific,
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

        // Compute output indexes of each section. GNU `--emit-relocs` puts each
        // copied RELA/REL header immediately after its target.
        let mut next_output_index = 0;
        let mut output_section_indexes = vec![None; output_sections.num_sections()];
        for id in output_section_id::section_header_order(output_order, output_sections) {
            if *keep_sections.get(id) {
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
        format_specific: &mut P::CommonGroupStateExt,
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

            P::allocate_internal_symbol(symbol_id, def_info, sizes, symbol_db, format_specific)?;
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
            start_stop_sections: input_state.start_stop_sections,
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
                &mut common.format_specific,
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
            &mut common.format_specific,
            |symbol_id, _info| {
                per_symbol_flags
                    .flags_for_symbol(symbol_id)
                    .has_resolution()
            },
        )?;

        Ok(())
    }
}
