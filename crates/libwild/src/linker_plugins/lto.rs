use crate::elf;
use crate::elf::Elf;
use crate::elf::ElfClass;
use crate::error::Result;
use crate::platform::Args as _;
use crate::platform::Platform;
use crate::resolution::ResolutionResources;
use crate::resolution::ResolvedFile;
use crate::resolution::ResolvedGroup;
use crate::resolution::SymbolAttributes;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::value_flags::FlagsForSymbol;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use rayon::Scope;

pub(crate) fn mark_lto_symbols_for_dynamic_export<C: ElfClass>(
    symbol_db: &SymbolDb<Elf<C>>,
    per_symbol_flags: &mut PerSymbolFlags,
    resolved_groups: &[ResolvedGroup<Elf<C>>],
) {
    use crate::grouping::Group;

    for group in resolved_groups {
        for file in &group.files {
            if let ResolvedFile::LtoInput(lto_input) = file {
                let Group::LtoInputs(files) = &symbol_db.groups[lto_input.file_id.group()] else {
                    unreachable!();
                };
                let file = &files[lto_input.file_id.file()];

                let Some(mode) = crate::layout::export_symbols_mode(symbol_db, &file.input_ref)
                else {
                    continue;
                };

                for (symbol_id, symbol) in file.symbols_iter() {
                    if symbol.is_definition()
                        && crate::layout::can_export_global_def(
                            symbol_db,
                            elf::convert_elf_visibility(object::elf::SymbolVisibility(
                                symbol.visibility,
                            )),
                            symbol_id,
                            per_symbol_flags.flags_for_symbol(symbol_id),
                            mode,
                        )
                    {
                        per_symbol_flags.set_flag(symbol_id, ValueFlags::EXPORT_DYNAMIC);
                    }
                }
            }
        }
    }
}

pub(crate) fn has_loaded_lto_input<P: Platform>(resolved_groups: &[ResolvedGroup<P>]) -> bool {
    resolved_groups.iter().any(|group| {
        group
            .files
            .iter()
            .any(|file| matches!(file, ResolvedFile::LtoInput(_)))
    })
}

pub(crate) fn resolve_lto_symbols<'data, 'scope, C: ElfClass>(
    obj: &crate::linker_plugins::LtoInput<'data>,
    resources: &'scope ResolutionResources<'data, 'scope, Elf<C>>,
    definitions_out: &mut [SymbolId],
    scope: &Scope<'scope>,
) -> Result {
    obj.symbols
        .iter()
        .enumerate()
        .zip(definitions_out)
        .try_for_each(
            |((local_symbol_index, local_symbol), definition)| -> Result {
                if !local_symbol.is_definition() {
                    let mut name_info = Elf::<C>::parse_raw_symbol_name(local_symbol.name.bytes());
                    if let Some(version) = local_symbol.version {
                        name_info.version_name = Some(version);
                    }

                    let symbol_attributes = SymbolAttributes {
                        name_info,
                        is_local: false,
                        default_visibility: local_symbol.visibility == object::elf::STV_DEFAULT.0,
                        is_weak: local_symbol.kind
                            == Some(crate::linker_plugins::SymbolKind::WeakUndef),
                    };

                    crate::resolution::resolve_symbol(
                        obj.symbol_id_range.offset_to_id(local_symbol_index),
                        &symbol_attributes,
                        definition,
                        resources,
                        false,
                        obj.file_id,
                        scope,
                        true,
                    )?;
                }

                Ok(())
            },
        )
}

/// Marks symbols related to --wrap as having non-IR references. This ensures that the linker
/// plugin preserves these symbols in its output rather than internalising them.
pub(crate) fn mark_wrap_symbols_as_non_ir_ref<'data, P: Platform>(
    symbol_db: &SymbolDb<'data, P>,
    per_symbol_flags: &mut PerSymbolFlags,
) {
    for name in symbol_db.args.symbol_names_to_wrap() {
        for lookup_name in [
            name.clone(),
            format!("__wrap_{name}"),
            format!("__real_{name}"),
        ] {
            if let Some(symbol_id) =
                symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(lookup_name.as_bytes()))
            {
                per_symbol_flags.set_flag(symbol_id, ValueFlags::HAS_NON_IR_REF);
            }
        }
    }
}
