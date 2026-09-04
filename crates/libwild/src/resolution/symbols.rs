use super::types::*;
use crate::debug_assert_bail;
use crate::error::Context as _;
use crate::error::Result;
use crate::grouping::Group;
use crate::grouping::SequencedInputObject;
use crate::hash::PassThroughHashMap;
use crate::hash::PreHashed;
use crate::input_data::FileId;
use crate::input_data::PRELUDE_FILE_ID;
use crate::linker_script::Expression;
use crate::output_section_id::OutputSections;
use crate::output_section_id::SectionName;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::SymbolPlacement;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::Symbol as _;
use crate::symbol::PreHashedSymbolName;
use crate::symbol::UnversionedSymbolName;
use crate::symbol::VersionedSymbolName;
use crate::symbol_db;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolStrength;
use crate::symbol_db::Visibility;
use crate::timing_phase;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use atomic_take::AtomicTake;
use rayon::Scope;

pub(crate) struct ResolutionResources<'data, 'scope, P: Platform> {
    pub(super) definitions_per_file: &'scope Vec<Vec<AtomicTake<&'scope mut [SymbolId]>>>,
    pub(super) symbol_db: &'scope SymbolDb<'data, P>,
    pub(super) outputs: &'scope Outputs<'data, P>,
    pub(super) per_symbol_flags: &'scope AtomicPerSymbolFlags<'scope>,
}

impl<'scope, 'data, P: Platform> ResolutionResources<'data, 'scope, P> {
    /// Request loading of `file_id` if it hasn't already been requested.
    #[inline(always)]
    pub(super) fn try_request_file_id(&'scope self, file_id: FileId, scope: &Scope<'scope>) {
        let definitions_group = &self.definitions_per_file[file_id.group()];

        let Some(atomic_take) = &definitions_group.get(file_id.file()) else {
            // A group from a previous resolution batch. Assume that the relevant file was already
            // loaded.
            return;
        };

        // Do a read before we call `take`. Reads are cheaper, so this is an optimisation that
        // reduces the need for exclusive access to the cache line.
        if atomic_take.is_taken() {
            // The definitions have previously been taken indicating that this file has already been
            // processed, nothing more to do.
            return;
        }

        let Some(definitions_out) = atomic_take.take() else {
            // Another thread just beat us to it.
            return;
        };

        work_items_do(
            file_id,
            definitions_out,
            self.symbol_db,
            self.outputs,
            |work_item| {
                scope.spawn(|scope| {
                    process_object(work_item, self, scope);
                });
            },
        );
    }

    pub(super) fn handle_result(&self, result: Result) {
        if let Err(error) = result {
            let _ = self.outputs.errors.push(error);
        }
    }
}

pub(super) fn work_items_do<'definitions, 'data, P: Platform>(
    file_id: FileId,
    mut definitions_out: &'definitions mut [SymbolId],
    symbol_db: &SymbolDb<'data, P>,
    outputs: &Outputs<'data, P>,
    mut request_callback: impl FnMut(LoadObjectSymbolsRequest<'definitions>),
) {
    match &symbol_db.groups[file_id.group()] {
        Group::Objects(parsed_input_objects) => {
            let obj = &parsed_input_objects[file_id.file()];
            let common = ResolvedCommon::new(obj);
            let resolved_object =
                if let Some(dynamic_tag_values) = obj.parsed.object.dynamic_tag_values() {
                    ResolvedFile::Dynamic(ResolvedDynamic::new(common, dynamic_tag_values))
                } else {
                    ResolvedFile::Object(ResolvedObject::new(common, obj.section_id_range))
                };
            // Push won't fail because we allocated enough space for all the objects.
            outputs.loaded.push(resolved_object).unwrap();
        }
        Group::StubLibraries(_) => {}
        #[cfg(all(feature = "plugins", unix))]
        Group::LtoInputs(lto_objects) => {
            let obj = &lto_objects[file_id.file()];
            // Push won't fail because we allocated enough space for all the LTO objects.
            outputs
                .loaded_lto_objects
                .push(ResolvedLtoInput {
                    file_id: obj.file_id,
                    symbol_id_range: obj.symbol_id_range,
                    section_id_range: obj.section_id_range,
                })
                .unwrap();

            request_callback(LoadObjectSymbolsRequest {
                file_id,
                symbol_start_offset: 0,
                definitions_out,
            });
            return;
        }
        _ => {}
    }

    let chunk_size = match &symbol_db.groups[file_id.group()] {
        Group::Objects(_) => MAX_SYMBOLS_PER_WORK_ITEM,
        _ => definitions_out.len(),
    };

    let mut symbol_start_offset = 0;
    loop {
        let len = chunk_size.min(definitions_out.len());
        let chunk_definitions_out = definitions_out.split_off_mut(..len).unwrap();

        let work_item = LoadObjectSymbolsRequest {
            file_id,
            definitions_out: chunk_definitions_out,
            symbol_start_offset,
        };
        request_callback(work_item);

        symbol_start_offset += len;
        if definitions_out.is_empty() {
            break;
        }
    }
}
pub(super) fn process_object<'scope, 'data: 'scope, 'definitions, P: Platform>(
    work_item: LoadObjectSymbolsRequest<'definitions>,
    resources: &'scope ResolutionResources<'data, 'scope, P>,
    scope: &Scope<'scope>,
) {
    let file_id = work_item.file_id;
    let definitions_out = work_item.definitions_out;

    match &resources.symbol_db.groups[file_id.group()] {
        Group::Prelude(prelude) => {
            verbose_timing_phase!("Resolve prelude symbols");

            load_prelude(prelude, definitions_out, resources, scope);
        }
        Group::Objects(parsed_input_objects) => {
            verbose_timing_phase!("Resolve object symbols");

            let obj = &parsed_input_objects[file_id.file()];

            resources.handle_result(
                resolve_symbols(
                    obj,
                    resources,
                    work_item.symbol_start_offset,
                    definitions_out,
                    scope,
                )
                .with_context(|| format!("Failed to resolve symbols in {obj}")),
            );
        }
        Group::StubLibraries(_) => {}
        Group::LinkerScripts(scripts) => {
            for script in scripts {
                for sym in &script.parsed.symbol_defs {
                    if let SymbolPlacement::Redirect(redirect) = &sym.placement {
                        load_symbols_in_redirect(resources, scope, redirect);
                    }
                }
            }
        }
        Group::SyntheticSymbols(_) => {}
        #[cfg(all(feature = "plugins", unix))]
        Group::LtoInputs(objects) => {
            let obj = &objects[file_id.file()];
            resources.handle_result(
                P::resolve_lto_symbols(obj, resources, definitions_out, scope)
                    .with_context(|| format!("Failed to resolve symbols in {obj}")),
            );
        }
    }
}
fn load_prelude<'scope, 'data, P: Platform>(
    prelude: &crate::parsing::Prelude<P>,
    definitions_out: &mut [SymbolId],
    resources: &'scope ResolutionResources<'data, 'scope, P>,
    scope: &Scope<'scope>,
) {
    // The start symbol could be defined within an archive entry. If it is, then we need to load
    // it. We don't currently store the resulting SymbolId, but instead look it up again during
    // layout. Skip when there is no entry (e.g. Wasm `--no-entry`).
    if let Some(entry_name) = resources.symbol_db.entry_symbol_name() {
        let symbol_id = load_symbol_named(resources, &mut SymbolId::undefined(), entry_name, scope);

        if let Some(symbol_id) = symbol_id {
            resources
                .per_symbol_flags
                .get_atomic(symbol_id)
                .fetch_or(ValueFlags::HAS_NON_IR_REF);
        }
    }

    // Try to resolve any symbols that the user requested be undefined (e.g. via --undefined). If an
    // object defines such a symbol, request that the object be loaded. Also, point our undefined
    // symbol record to the definition.
    for (def_info, definition_out) in prelude.symbol_definitions.iter().zip(definitions_out) {
        match &def_info.placement {
            SymbolPlacement::ForceUndefined => {
                load_symbol_named(resources, definition_out, def_info.name, scope);
            }
            SymbolPlacement::Redirect(redirect) => {
                load_symbols_in_redirect(resources, scope, redirect);
            }
            _ => {}
        }
    }
}

fn load_symbols_in_redirect<'data, 'scope, P: Platform>(
    resources: &'scope ResolutionResources<'data, 'scope, P>,
    scope: &Scope<'scope>,
    redirect: &crate::parsing::Redirect<'_>,
) {
    redirect.expression.visit_expressions(&mut |e| {
        if let Expression::Symbol(target_name) = e
            && let Some(target_symbol_id) = resources
                .symbol_db
                .get_unversioned(&UnversionedSymbolName::prehashed(target_name))
        {
            let file_id = resources.symbol_db.file_id_for_symbol(target_symbol_id);
            resources.try_request_file_id(file_id, scope);

            // Mark the target as having a non-IR reference. Without this, when the target is
            // defined in an LTO/IR input, the linker plugin would report the symbol as
            // `PrevailingDefIronly` and the LTO compiler would be free to DCE or internalize the
            // symbol, leaving the --defsym/script redirect with no resolution.
            resources
                .per_symbol_flags
                .get_atomic(target_symbol_id)
                .or_assign(ValueFlags::HAS_NON_IR_REF);
        }
        true
    });
}

fn load_symbol_named<'scope, 'data, P: Platform>(
    resources: &'scope ResolutionResources<'data, 'scope, P>,
    definition_out: &mut SymbolId,
    name: &[u8],
    scope: &Scope<'scope>,
) -> Option<SymbolId> {
    let symbol_id = resources
        .symbol_db
        .get_unversioned(&UnversionedSymbolName::prehashed(name));

    if let Some(symbol_id) = symbol_id {
        *definition_out = symbol_id;

        let symbol_file_id = resources.symbol_db.file_id_for_symbol(symbol_id);
        resources.try_request_file_id(symbol_file_id, scope);
    }

    symbol_id
}

/// Where there are multiple references to undefined symbols with the same name, pick one reference
/// as the canonical one to which we'll refer. Where undefined symbols can be resolved to
/// __start/__stop symbols that refer to the start or stop of a custom section, collect that
/// information up and put it into `custom_start_stop_defs`.
pub(super) fn canonicalise_undefined_symbols<'data, P: Platform>(
    mut undefined_symbols: Vec<UndefinedSymbol<'data>>,
    output_sections: &OutputSections<P>,
    groups: &[ResolvedGroup<'data, P>],
    symbol_db: &mut SymbolDb<'data, P>,
    per_symbol_flags: &mut PerSymbolFlags,
    custom_start_stop_defs: &mut ResolvedSyntheticSymbols<'data, P>,
) {
    timing_phase!("Canonicalise undefined symbols");

    let mut name_to_id: PassThroughHashMap<UnversionedSymbolName<'data>, SymbolId> =
        Default::default();

    let mut versioned_name_to_id: PassThroughHashMap<VersionedSymbolName<'data>, SymbolId> =
        Default::default();

    // Sort by symbol ID to ensure deterministic behaviour. We sort in reverse order so that LTO
    // outputs get higher priority than LTO inputs. This means that the canonical symbol ID for any
    // given name will be the one for the last file that refers to that symbol.
    undefined_symbols.sort_by_key(|u| usize::MAX - u.symbol_id.as_usize());

    for undefined in undefined_symbols {
        let is_defined = undefined.ignore_if_loaded.is_some_and(|file_id| {
            !matches!(
                groups[file_id.group()].files[file_id.file()],
                ResolvedFile::NotLoaded(_)
            )
        });

        if is_defined {
            // The archive entry that defined the symbol in question ended up being loaded, so the
            // weak symbol is defined after all.
            continue;
        }

        match undefined.name {
            PreHashedSymbolName::Unversioned(pre_hashed) => {
                match name_to_id.entry(pre_hashed) {
                    hashbrown::hash_map::Entry::Vacant(entry) => {
                        let symbol_id = allocate_start_stop_symbol_id(
                            pre_hashed,
                            symbol_db,
                            per_symbol_flags,
                            custom_start_stop_defs,
                            output_sections,
                        );

                        // We either make our undefined symbol dynamic, allowing the possibility
                        // that it might end up being defined at runtime, or we make it
                        // non-interposable, which means it'll remain null and even if it ends up
                        // defined at runtime, we won't use that definition. If the symbol doesn't
                        // have default visibility, then we make it non-interposable. If we're
                        // building a shared object, we always make the symbol dynamic. If we're
                        // building a statically linked executable, then we always make it
                        // non-interposable. If we're building a regular, dynamically linked
                        // executable, then we make it dynamic if the symbol is weak and otherwise
                        // make it non-interposable. That last case, a non-weak, default-visibility,
                        // undefined symbol in an executable is generally a link error, however if
                        // the flag --warn-unresolved-symbols is passed, then it won't be. Linker
                        // behaviour differs in this case. GNU ld makes the symbol non-interposable,
                        // while lld makes it dynamic. We match GNU ld in this case.
                        if symbol_id.is_none() {
                            let output_kind = symbol_db.output_kind;
                            let visibility = symbol_db.input_symbol_visibility(undefined.symbol_id);

                            if visibility == Visibility::Default
                                && (output_kind.is_shared_object()
                                    || (!output_kind.is_static_executable()
                                        && symbol_db.symbol_strength(undefined.symbol_id, groups)
                                            == SymbolStrength::Weak))
                            {
                                per_symbol_flags.set_flag(undefined.symbol_id, ValueFlags::DYNAMIC);
                            } else {
                                per_symbol_flags
                                    .set_flag(undefined.symbol_id, ValueFlags::NON_INTERPOSABLE);
                            }

                            if visibility != Visibility::Default
                                && let Some(def_id) = symbol_db.get_unversioned(&pre_hashed)
                            {
                                symbol_db::apply_visibility_to_definition(
                                    per_symbol_flags,
                                    symbol_db.definition(def_id),
                                    visibility,
                                );
                            }
                        }

                        // If the symbol isn't a start/stop symbol, then assign responsibility for
                        // the symbol to the first object that referenced
                        // it. This lets us have PLT/GOT entries
                        // for the symbol if they're needed.
                        let symbol_id = symbol_id.unwrap_or(undefined.symbol_id);
                        entry.insert(symbol_id);
                        symbol_db.replace_definition(undefined.symbol_id, symbol_id);
                    }
                    hashbrown::hash_map::Entry::Occupied(entry) => {
                        let definition_id = symbol_db.definition(*entry.get());
                        symbol_db.replace_definition(undefined.symbol_id, definition_id);
                        let visibility = symbol_db.input_symbol_visibility(undefined.symbol_id);
                        if visibility != Visibility::Default
                            && let Some(def_id) = symbol_db.get_unversioned(entry.key())
                        {
                            symbol_db::apply_visibility_to_definition(
                                per_symbol_flags,
                                symbol_db.definition(def_id),
                                visibility,
                            );
                        }
                    }
                }
            }
            PreHashedSymbolName::Versioned(pre_hashed) => {
                match versioned_name_to_id.entry(pre_hashed) {
                    hashbrown::hash_map::Entry::Vacant(entry) => {
                        entry.insert(undefined.symbol_id);
                    }
                    hashbrown::hash_map::Entry::Occupied(entry) => {
                        symbol_db.replace_definition(undefined.symbol_id, *entry.get());
                    }
                }
            }
        }
    }
}

fn allocate_start_stop_symbol_id<'data, P: Platform>(
    name: PreHashed<UnversionedSymbolName<'data>>,
    symbol_db: &mut SymbolDb<'data, P>,
    per_symbol_flags: &mut PerSymbolFlags,
    custom_start_stop_defs: &mut ResolvedSyntheticSymbols<'data, P>,
    output_sections: &OutputSections<P>,
) -> Option<SymbolId> {
    let symbol_name_bytes = name.bytes();

    let (section_name, is_start) = if let Some(s) = symbol_name_bytes.strip_prefix(b"__start_") {
        (s, true)
    } else {
        let s = symbol_name_bytes.strip_prefix(b"__stop_")?;
        (s, false)
    };

    let identity = P::section_identity_from_name(SectionName(section_name))?;
    let section_id = output_sections.custom_identity_to_id(identity)?;

    let def_info = if is_start {
        InternalSymDefInfo::new(SymbolPlacement::SectionStart(section_id), name.bytes())
    } else {
        InternalSymDefInfo::new(SymbolPlacement::SectionEnd(section_id), name.bytes())
    };

    let symbol_id = symbol_db.add_synthetic_symbol(per_symbol_flags, name, custom_start_stop_defs);

    custom_start_stop_defs.symbol_definitions.push(def_info);

    Some(symbol_id)
}
fn resolve_symbols<'data, 'scope, P: Platform>(
    obj: &SequencedInputObject<'data, P>,
    resources: &'scope ResolutionResources<'data, 'scope, P>,
    start_symbol_offset: usize,
    definitions_out: &mut [SymbolId],
    scope: &Scope<'scope>,
) -> Result {
    let verneed_table = obj.parsed.object.verneed_table()?;

    obj.parsed
        .object
        .symbols_iter()
        .skip(start_symbol_offset)
        .enumerate()
        .zip(definitions_out)
        .try_for_each(
            |((local_symbol_index, local_symbol), definition)| -> Result {
                // Don't try to resolve symbols that are already defined, e.g. locals and globals
                // that we define. Also skip the null symbol entry at index 0 for formats that
                // have one. Hidden symbols exported from shared objects don't make sense, so we
                // skip resolving them as well.
                if !definition.is_undefined()
                    || (P::HAS_NULL_SYMBOL_ENTRY && start_symbol_offset + local_symbol_index == 0)
                    || (obj.is_dynamic() && local_symbol.is_hidden())
                {
                    return Ok(());
                }

                let name_bytes = obj.parsed.object.symbol_name(local_symbol)?;

                let name_info = P::raw_symbol_name(
                    name_bytes,
                    &verneed_table,
                    object::SymbolIndex(local_symbol_index),
                );

                let symbol_attributes = SymbolAttributes {
                    name_info,
                    is_local: local_symbol.is_local(),
                    default_visibility: local_symbol.is_interposable(),
                    is_weak: local_symbol.is_weak(),
                };

                resolve_symbol(
                    obj.symbol_id_range
                        .offset_to_id(start_symbol_offset + local_symbol_index),
                    &symbol_attributes,
                    definition,
                    resources,
                    obj.is_dynamic(),
                    obj.file_id,
                    scope,
                    false,
                )
            },
        )
}

#[inline(always)]
pub(crate) fn resolve_symbol<'data, 'scope, P: Platform>(
    local_symbol_id: SymbolId,
    local_symbol_attributes: &SymbolAttributes<'data, P>,
    definition_out: &mut SymbolId,
    resources: &'scope ResolutionResources<'data, 'scope, P>,
    is_dynamic: bool,
    file_id: FileId,
    scope: &Scope<'scope>,
    from_ir: bool,
) -> Result {
    debug_assert_bail!(
        !local_symbol_attributes.is_local,
        "Only globals should be undefined, found symbol `{}` ({local_symbol_id})",
        local_symbol_attributes.name_info,
    );

    let prehashed_name = PreHashedSymbolName::from_raw(&local_symbol_attributes.name_info);

    // Only default-visibility symbols can reference symbols from shared objects.
    let allow_dynamic = local_symbol_attributes.default_visibility;

    match resources.symbol_db.get(&prehashed_name, allow_dynamic) {
        Some(symbol_id) => {
            *definition_out = symbol_id;
            // If the undefined reference has non-default visibility, the definition must be
            // downgraded so it cannot leak into dynsym
            if !local_symbol_attributes.default_visibility {
                let visibility = resources.symbol_db.input_symbol_visibility(local_symbol_id);
                match visibility {
                    Visibility::Hidden => {
                        resources.per_symbol_flags.get_atomic(symbol_id).or_assign(
                            ValueFlags::NON_INTERPOSABLE | ValueFlags::DOWNGRADE_TO_LOCAL,
                        );
                    }
                    Visibility::Protected => {
                        if !resources
                            .per_symbol_flags
                            .get_atomic(symbol_id)
                            .get()
                            .contains(ValueFlags::DYNAMIC)
                        {
                            resources
                                .per_symbol_flags
                                .get_atomic(symbol_id)
                                .or_assign(ValueFlags::NON_INTERPOSABLE);
                        }
                    }
                    Visibility::Default => {}
                }
            }

            if !from_ir {
                resources
                    .per_symbol_flags
                    .get_atomic(symbol_id)
                    .or_assign(ValueFlags::HAS_NON_IR_REF);
            }

            let symbol_file_id = resources.symbol_db.file_id_for_symbol(symbol_id);

            if symbol_file_id != file_id && !local_symbol_attributes.is_weak {
                // Undefined symbols in shared objects should actually activate as-needed shared
                // objects, however the rules for whether this should result in a DT_NEEDED entry
                // are kind of subtle, so for now, we don't activate shared objects from shared
                // objects. See
                // https://github.com/wild-linker/wild/issues/930#issuecomment-3007027924 for
                // more details. TODO: Fix this.
                if !is_dynamic || !resources.symbol_db.file(symbol_file_id).is_dynamic() {
                    resources.try_request_file_id(symbol_file_id, scope);
                }
            } else if symbol_file_id != PRELUDE_FILE_ID {
                // The symbol is weak and we can't be sure that the file that defined it will end up
                // being loaded, so the symbol might actually be undefined. Register it as an
                // undefined symbol then later when we handle undefined symbols, we'll check if the
                // file got loaded. TODO: If the file is a non-archived object, or possibly even if
                // it's an archived object that we've already decided to load, then we could skip
                // this.
                resources.outputs.undefined_symbols.push(UndefinedSymbol {
                    ignore_if_loaded: Some(symbol_file_id),
                    name: prehashed_name,
                    symbol_id: local_symbol_id,
                });
            }
        }
        None => {
            resources.outputs.undefined_symbols.push(UndefinedSymbol {
                ignore_if_loaded: None,
                name: prehashed_name,
                symbol_id: local_symbol_id,
            });
        }
    }
    Ok(())
}
