use super::*;
use crate::alignment::Alignment;
use crate::bail;
use crate::debug_assert_bail;
use crate::error::Context;
use crate::error::Error;
use crate::error::Result;
use crate::input_data::FileId;
use crate::layout::EnginePlatform;
use crate::layout::graph::*;
use crate::layout::sizes::*;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::SectionAttributes as _;
use crate::platform::Symbol as _;
use crate::resolution::SectionSlot;
use crate::symbol_db::SymbolDebug;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolIdRange;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::FlagsForSymbol as _;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use rayon::Scope;
use std::fmt::Display;
use std::mem::size_of;
use std::mem::swap;
use std::mem::take;
use std::sync::atomic;

pub(crate) trait HandlerData {
    fn symbol_id_range(&self) -> SymbolIdRange;

    fn file_id(&self) -> FileId;
}

pub(crate) trait SymbolRequestHandler<'data, P: EnginePlatform>:
    std::fmt::Display + HandlerData
{
    fn finalise_symbol_sizes<A: Arch<Platform = P>>(
        &mut self,
        common: &mut CommonGroupState<'data, P>,
        symbol_flags: &AtomicPerSymbolFlags,
        resources: &FinaliseSizesResources<'data, '_, P>,
    ) -> Result {
        let symbol_db = resources.symbol_db;

        let _file_span = crate::debug_trace::span_for_file(symbol_db.args, self.file_id());
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

            if symbol_db.args.verify_allocation_consistency() {
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

impl<'data, P: EnginePlatform> SymbolRequestHandler<'data, P> for ObjectLayoutState<'data, P> {
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

impl<'data, P: EnginePlatform> SymbolRequestHandler<'data, P> for DynamicLayoutState<'data, P> {
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

impl<'data, P: EnginePlatform> SymbolRequestHandler<'data, P> for PreludeLayoutState<'data, P> {
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

impl<'data, P: EnginePlatform> SymbolRequestHandler<'data, P>
    for LinkerScriptLayoutState<'data, P>
{
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

impl<'data, P: EnginePlatform> SymbolRequestHandler<'data, P> for StubLibraryLayoutState<'data, P> {
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

impl<'data, P: EnginePlatform> SymbolRequestHandler<'data, P>
    for SyntheticSymbolsLayoutState<'data, P>
{
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

        if let Some(output_section_id) = def_info.section_id()
            && let Some(start_stop_sections) = &mut self.start_stop_sections
        {
            // We've gotten a request to load a __start_ / __stop_ symbol, send requests to load all
            // sections that would go into that section.
            for candidate in take(start_stop_sections.get_mut(output_section_id)) {
                let request = GcLoadRequest::new(candidate.file_id, candidate.gc_unit);
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

impl<'data, P: EnginePlatform> CommonGroupState<'data, P> {
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

impl<'data, P: EnginePlatform> GroupActivationInputs<'data, P> {
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

        for file in &mut group.files {
            let r = activate::<A>(&mut group.common, file, &mut group.queue, resources, scope)
                .with_context(|| format!("Failed to activate {file}"));

            if let Err(error) = r {
                resources.errors.lock().unwrap().push(error);
            }
        }

        group.do_pending_work::<A>(resources, scope);
    }
}

impl<'data, P: EnginePlatform> GroupState<'data, P> {
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

impl<P: EnginePlatform> LocalWorkQueue<P> {
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

impl<'data, P: EnginePlatform> GraphResources<'data, '_, P> {
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

impl<'data, P: EnginePlatform> FileLayoutState<'data, P> {
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
                FileLayoutState::Dynamic(state) => P::copy_relocate_symbol(
                    state,
                    symbol_id,
                    crate::layout::platform_graph(resources),
                ),

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
                    crate::layout::platform_graph(resources),
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

    pub(crate) fn finalise_layout<'scope, 'writer, 'out>(
        self,
        memory_offsets: &mut OutputSectionPartMap<u64>,
        resolutions_out: &'writer mut sharded_vec_writer::Shard<'out, Option<Resolution<P>>>,
        resources: &FinaliseLayoutResources<'scope, 'data, P>,
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
    pub(crate) fn create<'data, P: EnginePlatform>(
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
    pub(crate) fn capacity<P: EnginePlatform>(
        self,
        part_id: PartId,
        output_sections: &OutputSections<P>,
    ) -> u64 {
        if part_id.should_pack::<P>() {
            self.size
        } else {
            output_sections
                .part_alignment::<P>(part_id)
                .align_up(self.size)
        }
    }

    pub(crate) fn place(self, offset: u64) -> (u64, u64) {
        let address = self.alignment.align_up(offset);
        (address, address + self.size)
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

impl<P: EnginePlatform> GcLoadRequest<P> {
    pub(crate) fn new(file_id: FileId, gc_unit: P::GcUnit) -> Self {
        Self { file_id, gc_unit }
    }
}

/// An input section that needs to be sorted due to a `SORT*` directive or `--sort-section`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct InputSortedSection {
    pub(crate) file_id: FileId,
    pub(crate) section_index: object::SectionIndex,
    pub(crate) part_id: PartId,
    pub(crate) size: u64,
    pub(crate) alignment: Alignment,
}

pub(crate) fn assign_addresses_to_sorted_sections<P: EnginePlatform>(
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
