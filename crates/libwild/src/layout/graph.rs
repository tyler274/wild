use super::types::*;
use crate::OutputKind;
use crate::error;
use crate::error::Context;
use crate::error::Error;
use crate::error::Result;
use crate::input_data::InputRef;
use crate::layout_rules::SectionKind;
use crate::linker_script::Expression;
use crate::output_section_id::OutputSections;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::SymbolPlacement;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::ProgramSegmentDef as _;
use crate::platform::Symbol as _;
use crate::resolution;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::symbol_db::Visibility;
use crate::thunks;
use crate::thunks::ThunkBlockId;
use crate::timing_phase;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::FlagsForSymbol as _;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use linker_utils::elf::RelocationKind;
use linker_utils::relaxation::RelaxDeltaMap;
use rayon::Scope;
use std::mem::take;
use std::sync::Mutex;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;

pub(crate) fn export_dynamic<'data, P: Platform>(
    common: &mut CommonGroupState<'data, P>,
    symbol_id: SymbolId,
    symbol_db: &SymbolDb<'data, P>,
) -> Result {
    common
        .dynamic_symbol_definitions
        .push(P::create_dynamic_symbol_definition(symbol_db, symbol_id)?);

    Ok(())
}

/// Traverse the graph of references. This is where we garbage-collect unused stuff if enabled. Even
/// when GC isn't enabled, we still run this, since we perform size calculations during this phase.
pub(crate) fn traverse_reference_graph<'data, A: Arch>(
    groups_in: Vec<resolution::ResolvedGroup<'data, A::Platform>>,
    symbol_db: &SymbolDb<'data, A::Platform>,
    per_symbol_flags: &AtomicPerSymbolFlags,
    output_sections: &OutputSections<'data, A::Platform>,
    layout_resources_ext: <A::Platform as Platform>::LayoutResourcesExt<'data>,
) -> Result<GcOutputs<'data, A::Platform>> {
    timing_phase!("Traverse reference graph");

    let num_groups = groups_in.len();

    let thunk_layout_builder = thunks::ThunkLayoutBuilder::new::<A>(&groups_in);

    let mut worker_slots = Vec::with_capacity(num_groups);
    worker_slots.resize_with(num_groups, || {
        Mutex::new(WorkerSlot {
            work: Default::default(),
            worker: None,
        })
    });

    let resources = GraphResources {
        symbol_db,
        output_sections,
        worker_slots,
        errors: Mutex::new(Vec::new()),
        per_symbol_flags,
        must_keep_sections: output_sections.new_section_map(),
        has_static_tls: AtomicBool::new(false),
        has_variant_pcs: AtomicBool::new(false),
        thunk_layout_builder,
        layout_resources_ext,
    };
    let resources_ref = &resources;

    rayon::in_place_scope(|scope| {
        queue_initial_group_processing::<A>(groups_in, symbol_db, resources_ref, scope);
    });

    let mut errors: Vec<Error> = take(resources.errors.lock().unwrap().as_mut());
    // TODO: Figure out good way to report more than one error.
    if let Some(error) = errors.pop() {
        return Err(error);
    }

    let mut group_states = unwrap_worker_states(&resources.worker_slots);

    <A::Platform as Platform>::post_gc(&mut group_states, symbol_db)?;

    // Give our prelude a chance to tie up a few last sizes while we still have access to
    // `resources`.
    let prelude_group = &mut group_states[0];
    let FileLayoutState::Prelude(prelude) = &mut prelude_group.files[0] else {
        unreachable!("Prelude must be first");
    };

    <A::Platform as Platform>::pre_finalise_sizes_prelude(
        prelude,
        &mut prelude_group.common,
        &resources,
    );

    let must_keep_sections = resources.must_keep_sections.into_map(|v| v.into_inner());

    Ok(GcOutputs {
        group_states,
        must_keep_sections,
        has_static_tls: resources.has_static_tls.load(atomic::Ordering::Relaxed),
        has_variant_pcs: resources.has_variant_pcs.load(atomic::Ordering::Relaxed),
        thunk_layout_builder: resources.thunk_layout_builder,
    })
}

pub(crate) fn queue_initial_group_processing<'data, 'scope, A: Arch>(
    groups_in: Vec<resolution::ResolvedGroup<'data, A::Platform>>,
    symbol_db: &'scope SymbolDb<'data, A::Platform>,
    resources: &'scope GraphResources<'data, '_, A::Platform>,
    scope: &Scope<'scope>,
) {
    verbose_timing_phase!("Create worker slots");

    assert_eq!(groups_in.len(), symbol_db.groups.len());

    groups_in
        .into_iter()
        .enumerate()
        .zip(&symbol_db.groups)
        .for_each(|((group_index, resolved), group)| {
            scope.spawn(move |scope| {
                verbose_timing_phase!("Activate group");
                let inputs = GroupActivationInputs {
                    resolved,
                    num_symbols: group.num_symbols(),
                    group_index,
                };
                inputs.activate_group::<A>(resources, scope);
            });
        });
}

pub(crate) fn unwrap_worker_states<'data, P: Platform>(
    worker_slots: &[Mutex<WorkerSlot<'data, P>>],
) -> Vec<GroupState<'data, P>> {
    worker_slots
        .iter()
        .filter_map(|w| w.lock().unwrap().worker.take())
        .collect()
}

pub(crate) fn activate<'data, 'scope, A: Arch>(
    common: &mut CommonGroupState<'data, A::Platform>,
    file: &mut FileLayoutState<'data, A::Platform>,
    queue: &mut LocalWorkQueue<A::Platform>,
    resources: &'scope GraphResources<'data, '_, A::Platform>,
    scope: &Scope<'scope>,
) -> Result {
    match file {
        FileLayoutState::Object(s) => s.activate::<A>(common, resources, queue, scope)?,
        FileLayoutState::Prelude(s) => s.activate::<A>(common, resources, queue, scope)?,
        FileLayoutState::Dynamic(s) => s.activate::<A>(common, resources, queue, scope)?,
        FileLayoutState::LinkerScript(s) => s.activate::<A>(common, resources, queue, scope)?,
        FileLayoutState::Epilogue(_) => {}
        FileLayoutState::StubLibrary(_) => {}
        FileLayoutState::NotLoaded(_) => {}
        FileLayoutState::SyntheticSymbols(_) => {}
    }
    Ok(())
}

pub(crate) fn resolution_flags(rel_kind: RelocationKind) -> ValueFlags {
    match rel_kind {
        RelocationKind::PltRelative | RelocationKind::PltRelGotBase => {
            ValueFlags::PLT | ValueFlags::GOT
        }
        RelocationKind::Got
        | RelocationKind::GotRelGotBase
        | RelocationKind::GotRelative
        | RelocationKind::GotRelativeLoongArch64 => ValueFlags::GOT,
        RelocationKind::GotTpOff
        | RelocationKind::GotTpOffLoongArch64
        | RelocationKind::GotTpOffGot
        | RelocationKind::GotTpOffGotBase => ValueFlags::GOT_TLS_OFFSET,
        RelocationKind::TlsGd | RelocationKind::TlsGdGot | RelocationKind::TlsGdGotBase => {
            ValueFlags::GOT_TLS_MODULE
        }
        RelocationKind::TlsDesc
        | RelocationKind::TlsDescLoongArch64
        | RelocationKind::TlsDescGot
        | RelocationKind::TlsDescGotBase
        | RelocationKind::TlsDescCall => ValueFlags::GOT_TLS_DESCRIPTOR,
        RelocationKind::TlsLd | RelocationKind::TlsLdGot | RelocationKind::TlsLdGotBase => {
            ValueFlags::empty()
        }
        RelocationKind::Absolute
        | RelocationKind::AbsoluteSet
        | RelocationKind::AbsoluteSetWord6
        | RelocationKind::AbsoluteAddition
        | RelocationKind::AbsoluteAdditionWord6
        | RelocationKind::AbsoluteSubtraction
        | RelocationKind::AbsoluteSubtractionWord6
        | RelocationKind::Relative
        | RelocationKind::RelativeRiscVLow12
        | RelocationKind::RelativeLoongArchHigh
        | RelocationKind::DtpOff
        | RelocationKind::TpOff
        | RelocationKind::SymRelGotBase
        | RelocationKind::PairSubtractionULEB128(..) => ValueFlags::DIRECT,
        RelocationKind::None | RelocationKind::AbsoluteLowPart | RelocationKind::Alignment => {
            ValueFlags::empty()
        }
    }
}

pub(crate) fn load_redirect_referenced_symbols<'data, 'scope, A: Arch>(
    resources: &'scope GraphResources<'data, '_, <A as Arch>::Platform>,
    queue: &mut LocalWorkQueue<A::Platform>,
    scope: &Scope<'scope>,
    symbol_id: SymbolId,
    redirect: &crate::parsing::Redirect<'data>,
) {
    resources
        .per_symbol_flags
        .get_atomic(symbol_id)
        .or_assign(ValueFlags::DIRECT);

    load_expression_referenced_symbols::<A>(resources, queue, scope, &redirect.expression);
}

pub(crate) fn load_expression_referenced_symbols<'data, 'scope, A: Arch>(
    resources: &'scope GraphResources<'data, '_, <A as Arch>::Platform>,
    queue: &mut LocalWorkQueue<A::Platform>,
    scope: &Scope<'scope>,
    expression: &Expression<'data>,
) {
    // Also mark any symbols in the expression as used and queue it for loading to
    // prevent it from being GC'd.
    expression.visit_expressions(&mut |e| {
        if let crate::linker_script::Expression::Symbol(target_name) = e
            && let Some(target_symbol_id) = resources
                .symbol_db
                .get_unversioned(&UnversionedSymbolName::prehashed(target_name))
        {
            let canonical_target_id = resources.symbol_db.definition(target_symbol_id);
            let file_id = resources.symbol_db.file_id_for_symbol(canonical_target_id);
            let old_flags = resources
                .per_symbol_flags
                .get_atomic(canonical_target_id)
                .fetch_or(ValueFlags::DIRECT);

            if !old_flags.has_resolution() {
                queue.send_work::<A>(
                    resources,
                    file_id,
                    WorkItem::LoadGlobalSymbol(canonical_target_id),
                    scope,
                );
            }
        }
        true
    });
}

pub(crate) fn load_redirect_expression_targets<'data, 'scope, A: Arch>(
    resources: &'scope GraphResources<'data, '_, <A as Arch>::Platform>,
    queue: &mut LocalWorkQueue<A::Platform>,
    scope: &Scope<'scope>,
    redirect: &crate::parsing::Redirect<'data>,
) {
    load_expression_referenced_symbols::<A>(resources, queue, scope, &redirect.expression);
}

pub(crate) fn provide_has_missing_rhs<'data, P: Platform>(
    def_info: &InternalSymDefInfo<'data, P>,
    symbol_db: &SymbolDb<'data, P>,
) -> bool {
    let SymbolPlacement::Redirect(redirect) = &def_info.placement else {
        return false;
    };
    let mut missing = false;
    redirect.expression.visit_expressions(&mut |e| {
        if let crate::linker_script::Expression::Symbol(name) = e
            && symbol_db
                .get_unversioned(&UnversionedSymbolName::prehashed(name))
                .is_none()
        {
            missing = true;
        }
        true
    });
    missing
}

pub(crate) fn create_internal_symbol_resolution<'data, P: Platform>(
    memory_offsets: &mut OutputSectionPartMap<u64>,
    resources: &FinaliseLayoutResources<'_, 'data, P>,
    def_info: &InternalSymDefInfo<P>,
    symbol_id: SymbolId,
) -> Option<Resolution<P>> {
    if def_info.name.is_empty() || !resources.symbol_db.is_canonical(symbol_id) {
        return None;
    }

    if !resources
        .per_symbol_flags
        .flags_for_symbol(symbol_id)
        .has_resolution()
    {
        return None;
    }

    let raw_value = match def_info.placement {
        SymbolPlacement::Undefined
        | SymbolPlacement::ForceUndefined
        | SymbolPlacement::PlatformSpecific(_) => 0,
        SymbolPlacement::SectionStart(section_id) => {
            resources.section_layouts.get(section_id).mem_offset
        }
        SymbolPlacement::SectionEnd(section_id) => {
            let sec = resources.section_layouts.get(section_id);
            sec.mem_offset + sec.mem_size
        }
        SymbolPlacement::SectionGroupEnd(section_id) => {
            let mut end = {
                let sec = resources.section_layouts.get(section_id);
                sec.mem_offset + sec.mem_size
            };

            for (id, info) in resources.output_sections.ids_with_info() {
                if let SectionKind::Secondary(primary_id) = info.kind
                    && primary_id == section_id
                {
                    let sec = resources.section_layouts.get(id);
                    let candidate_end = sec.mem_offset + sec.mem_size;
                    if candidate_end > end {
                        end = candidate_end;
                    }
                }
            }
            end
        }
        SymbolPlacement::Redirect(_) => {
            // For redirects to other symbols, we defer resolution until later when all symbols have
            // been resolved. This is handled by update_redirect_resolutions() which is called after
            // layout is complete.
            0
        }
        SymbolPlacement::LoadBaseAddress => resources
            .segment_layouts
            .segments
            .iter()
            .find(|seg| resources.program_segments.segment_def(seg.id).is_loadable())
            .map(|seg| seg.sizes.mem_offset)?,
    };

    Some(P::create_resolution(
        resources
            .symbol_db
            .flags_for_symbol(resources.per_symbol_flags, symbol_id),
        raw_value,
        None,
        memory_offsets,
        resources.symbol_db.args,
        resources.symbol_db.output_kind,
    ))
}

/// Emits an undefined symbol error or warning if applicable.
pub(crate) fn check_for_undefined<A: Arch>(
    object: &ObjectLayoutState<A::Platform>,
    section: &<A::Platform as Platform>::SectionHeader,
    rel_offset: u64,
    local_sym_index: object::SymbolIndex,
    flags: ValueFlags,
    symbol_id: SymbolId,
    resources: &GraphResources<A::Platform>,
) -> Result {
    let symbol_db = resources.symbol_db;

    if !should_emit_undefined_error(object, local_sym_index, flags, symbol_id, symbol_db) {
        return Ok(());
    }

    let symbol_name = symbol_db.symbol_name_for_display(symbol_id);
    let source_info = A::get_source_info(object.object, &object.relocations, section, rel_offset)
        .context("Failed to get source info")?;

    if symbol_db.args.should_error_on_unresolved_symbols() {
        resources.report_error(error!(
            "Undefined symbol {symbol_name}, referenced by {}\n    {}",
            source_info, object.input,
        ));
    } else {
        resources.symbol_db.warning(format!(
            "Undefined symbol {symbol_name}, referenced by {}\n    {}",
            source_info, object.input,
        ));
    }

    Ok(())
}

pub(crate) fn should_emit_undefined_error<P: Platform>(
    object: &ObjectLayoutState<P>,
    local_sym_index: object::SymbolIndex,
    flags: ValueFlags,
    symbol_id: SymbolId,
    symbol_db: &SymbolDb<P>,
) -> bool {
    // We always mark undefined symbols with the absolute flag, so if that's not set, we know the
    // symbol isn't undefined and we can save other checks.
    if !flags.is_absolute() {
        return false;
    }

    let Ok(local_symbol) = object.object.symbol(local_sym_index) else {
        // If we can't read the symbol, the error will be reported elsewhere.
        return false;
    };

    if symbol_db
        .args
        .should_allow_object_undefined(symbol_db.output_kind)
        || local_symbol.is_weak()
    {
        return false;
    }

    match symbol_db.args.unresolved_symbols_behaviour() {
        crate::args::UnresolvedSymbols::IgnoreAll
        | crate::args::UnresolvedSymbols::IgnoreInObjectFiles => false,
        _ => symbol_db.is_undefined(symbol_id),
    }
}

/// Construct a new inactive instance, which means we don't yet load non-GC sections and only
/// load them later if a symbol from this object is referenced.
pub(crate) fn new_object_layout_state<P: Platform>(
    input_state: resolution::ResolvedObject<P>,
) -> FileLayoutState<P> {
    // Note, this function is called for all objects from a single thread, so don't be tempted to do
    // significant work here. Do work when activate is called instead. Doing it there also means
    // that we don't do the work unless the object is actually needed.

    FileLayoutState::Object(ObjectLayoutState {
        file_id: input_state.common.file_id,
        symbol_id_range: input_state.common.symbol_id_range,
        section_id_range: input_state.section_id_range,
        input: input_state.common.input,
        object: input_state.common.object,
        link_order: input_state.common.link_order,
        sections: input_state.sections,
        relocations: input_state.relocations,
        format_specific: P::new_object_layout_state_ext(input_state.format_specific),
        section_relax_deltas: RelaxDeltaMap::new(),
        script_sorted_sections: input_state.script_sorted_sections,
        thunk_block_id: ThunkBlockId::default(),
        owns_thunk_block: false,
        post_gc_primary_bytes: 0,
    })
}

pub(crate) fn new_dynamic_object_layout_state<'data, P: Platform>(
    input_state: &resolution::ResolvedDynamic<'data, P>,
    args: &P::Args,
) -> FileLayoutState<'data, P> {
    FileLayoutState::Dynamic(DynamicLayoutState {
        file_id: input_state.common.file_id,
        symbol_id_range: input_state.common.symbol_id_range,
        lib_name: input_state.lib_name(),
        object: input_state.common.object,
        input: input_state.common.input,
        format_specific: P::new_dynamic_layout_state_ext(input_state, args),
    })
}

pub(crate) fn export_symbols_mode<P: Platform>(
    symbol_db: &SymbolDb<P>,
    input: &InputRef,
) -> Option<ExportSymbolsMode> {
    if symbol_db.output_kind == OutputKind::SharedObject
        && (!input.has_archive_semantics()
            || symbol_db.args.should_export_dynamic(input.lib_name()))
    {
        return Some(ExportSymbolsMode::All);
    }

    if symbol_db.output_kind.needs_dynsym() && symbol_db.args.should_export_all_dynamic_symbols() {
        return Some(ExportSymbolsMode::All);
    }

    if symbol_db.output_kind.needs_dynsym() && symbol_db.export_list.is_some() {
        return Some(ExportSymbolsMode::Selected);
    }

    None
}

pub(crate) fn can_export_symbol<P: Platform>(
    sym: &P::SymtabEntry,
    symbol_id: SymbolId,
    resources: &GraphResources<P>,
    mode: ExportSymbolsMode,
) -> bool {
    if sym.is_undefined() || sym.is_local() {
        return false;
    }

    let flags = resources.local_flags_for_symbol(symbol_id);

    can_export_global_def(
        resources.symbol_db,
        sym.visibility(),
        symbol_id,
        flags,
        mode,
    )
}

pub(crate) fn can_export_global_def<P: Platform>(
    symbol_db: &SymbolDb<P>,
    visibility: Visibility,
    symbol_id: SymbolId,
    flags: ValueFlags,
    mode: ExportSymbolsMode,
) -> bool {
    if visibility == Visibility::Hidden {
        return false;
    }

    if !symbol_db.is_canonical(symbol_id) {
        return false;
    }

    if flags.is_downgraded_to_local() {
        return false;
    }

    if mode == ExportSymbolsMode::Selected
        && let Some(export_list) = &symbol_db.export_list
        && let Ok(symbol_name) = symbol_db.symbol_name(symbol_id)
        && !&export_list.contains(&UnversionedSymbolName::prehashed(symbol_name.bytes()))
    {
        return false;
    }

    true
}
