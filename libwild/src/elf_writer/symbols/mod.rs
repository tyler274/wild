mod table;
mod versions;

use super::types::*;
use crate::bail;
use crate::debug_assert_bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::Versym;
use crate::elf::output_section_id;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::DynamicLayout;
use crate::layout::FileLayout;
use crate::layout::LinkerScriptLayoutState;
use crate::layout::ObjectLayout;
use crate::layout::PreludeLayout;
use crate::platform;
use crate::platform::ObjectFile;
use crate::symbol_db::SymbolId;
use crate::value_flags::ValueFlags;
use crate::writable_elf::WritableSymbol as _;
use object::read::elf::Sym as _;
use rayon::iter::ParallelBridge as _;
use rayon::iter::ParallelIterator as _;
#[allow(unused_imports)]
pub(crate) use table::*;
#[allow(unused_imports)]
pub(crate) use versions::*;

pub(crate) struct VersionedDynsymWriter<'layout, 'out, C: ElfClass> {
    pub(crate) dynsym_writer: SymbolTableWriter<'layout, 'out, C>,
    pub(crate) versym: Option<&'out mut [Versym]>,
}

pub(crate) fn write_dynamic_symbol_definitions<C: ElfClass>(
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
) -> Result {
    let chunk_size =
        10.max(layout.dynamic_symbol_definitions.len() / 10 / rayon::current_num_threads());

    layout
        .dynamic_symbol_definitions
        .chunks(chunk_size)
        .map(|defs| (defs, table_writer.take_dynsym_prefix(defs)))
        .par_bridge()
        .try_for_each(|(defs, mut table_writer)| {
            for sym_def in defs {
                let file_id = layout.symbol_db.file_id_for_symbol(sym_def.symbol_id);
                let file_layout = &layout.file_layout(file_id);
                match file_layout {
                    FileLayout::Object(object) => {
                        write_regular_object_dynamic_symbol_definition(
                            sym_def,
                            object,
                            layout,
                            &mut table_writer.dynsym_writer,
                        )?;

                        if let Some(versym) = table_writer.versym.as_mut() {
                            write_symbol_version(versym, sym_def.format_specific.version)?;
                        }
                    }
                    FileLayout::Dynamic(object) => {
                        if layout
                            .flags_for_symbol(sym_def.symbol_id)
                            .needs_canonical_plt()
                        {
                            write_canonical_plt_dynamic_symbol_definition(
                                sym_def,
                                object,
                                layout,
                                &mut table_writer.dynsym_writer,
                            )?;
                        } else {
                            write_copy_relocation_dynamic_symbol_definition(
                                sym_def,
                                object,
                                layout,
                                &mut table_writer.dynsym_writer,
                            )?;
                        }

                        if let Some(versym) = table_writer.versym.as_mut() {
                            copy_symbol_version(
                                object.object.symbol_versions(),
                                object.symbol_id_range.id_to_offset(sym_def.symbol_id),
                                &object.format_specific.version_mapping,
                                versym,
                            )?;
                        }
                    }
                    FileLayout::LinkerScript(script) => {
                        write_linker_script_dynsym(
                            &mut table_writer.dynsym_writer,
                            layout,
                            sym_def.symbol_id,
                            script,
                        )
                        .with_context(|| {
                            format!(
                                "Failed to write linker script dynsym: {}",
                                layout.symbol_debug(sym_def.symbol_id)
                            )
                        })?;
                    }
                    FileLayout::Prelude(prelude) => {
                        write_prelude_dynsym(
                            &mut table_writer.dynsym_writer,
                            layout,
                            sym_def.symbol_id,
                            prelude,
                        )?;
                        if let Some(versym) = table_writer.versym.as_mut() {
                            write_symbol_version(versym, sym_def.format_specific.version)?;
                        }
                    }
                    _ => bail!(
                        "Internal error: Unexpected dynamic symbol definition from {:?}. {}",
                        file_layout,
                        layout.symbol_debug(sym_def.symbol_id)
                    ),
                }
            }

            Ok(())
        })
}

/// Writes a symbol that was produced by a linker script.
pub(crate) fn write_linker_script_dynsym<C: ElfClass>(
    dynsym_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
    script: &LinkerScriptLayoutState<elf::Elf<C>>,
) -> Result {
    let local_index = script
        .internal_symbols
        .symbol_id_range()
        .id_to_offset(symbol_id);
    let info = &script.internal_symbols.symbol_definitions[local_index];
    write_internal_dynsym(dynsym_writer, layout, symbol_id, info)
}

/// Get the section index and type for a symbol.
/// This is used to copy attributes from a target symbol to a defsym alias.
pub(crate) fn write_prelude_dynsym<C: ElfClass>(
    dynsym_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
    prelude: &PreludeLayout<elf::Elf<C>>,
) -> Result {
    let offset = symbol_id.offset_from(prelude.internal_symbols.start_symbol_id);
    let def_info = prelude
        .internal_symbols
        .symbol_definitions
        .get(offset)
        .with_context(|| format!("Invalid prelude symbol {}", layout.symbol_debug(symbol_id)))?;
    write_internal_dynsym(dynsym_writer, layout, symbol_id, def_info)
}

pub(crate) fn write_internal_dynsym<C: ElfClass>(
    dynsym_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
    def_info: &crate::parsing::InternalSymDefInfo<elf::Elf<C>>,
) -> Result {
    if matches!(
        def_info.placement,
        crate::parsing::SymbolPlacement::Redirect(_)
    ) {
        return write_defsym_dynsym(dynsym_writer, layout, symbol_id, def_info);
    }

    let section_id = def_info
        .section_id()
        .context("Tried to export dynamic symbol not associated with a section")?;

    let section_id = layout.output_sections.primary_output_section(section_id);

    let shndx = layout
        .output_sections
        .output_index_of_section(section_id)
        .context("Tried to write dynamic symbol in section that's not being output")?;

    let resolution = layout
        .local_symbol_resolution(symbol_id)
        .with_context(|| format!("Missing resolution for {}", layout.symbol_debug(symbol_id)))?;

    let address = resolution.address()?;
    let name = layout.symbol_db.symbol_name(symbol_id)?;

    let entry = dynsym_writer.define_symbol(
        false,
        SymbolSection::Index(shndx),
        address,
        0,
        Some(name.bytes()),
    )?;
    entry.set_binding_and_type(object::elf::STB_GLOBAL, object::elf::STT_NOTYPE);

    Ok(())
}

/// Writes a dynsym entry for a symbol defined via --defsym or linker script symbol assignment.
pub(crate) fn write_defsym_dynsym<C: ElfClass>(
    dynsym_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
    def_info: &crate::parsing::InternalSymDefInfo<elf::Elf<C>>,
) -> Result {
    let resolution = layout
        .local_symbol_resolution(symbol_id)
        .with_context(|| format!("Missing resolution for {}", layout.symbol_debug(symbol_id)))?;
    let address = resolution.raw_value;
    let (shndx, st_type) = get_defsym_attributes(layout, def_info, address)?;
    let name = layout.symbol_db.symbol_name(symbol_id)?;

    let entry = dynsym_writer
        .define_symbol(false, shndx, address, 0, Some(name.bytes()))
        .with_context(|| {
            format!(
                "Failed to define dynamic {}",
                layout.symbol_debug(symbol_id)
            )
        })?;
    entry.set_binding_and_type(object::elf::STB_GLOBAL, st_type);

    Ok(())
}

pub(crate) fn write_copy_relocation_dynamic_symbol_definition<'data, C: ElfClass>(
    sym_def: &crate::layout::DynamicSymbolDefinition<elf::Elf<C>>,
    object: &DynamicLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
    dynamic_symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
) -> Result {
    debug_assert_bail!(
        layout
            .flags_for_symbol(sym_def.symbol_id)
            .needs_copy_relocation(),
        "Tried to write copy relocation for symbol without COPY_RELOCATION flag"
    );
    let sym_index = sym_def.symbol_id.to_input(object.symbol_id_range);
    let sym = object.object.symbol(sym_index)?;
    let name = sym_def.name;
    let shndx = layout
        .output_sections
        .output_index_of_section(output_section_id::BSS)
        .context("Copy relocation with no BSS section")?;
    let res = layout
        .local_symbol_resolution(sym_def.symbol_id)
        .context("Copy relocation for unresolved symbol")?;
    dynamic_symbol_writer
        .copy_symbol_shndx(sym, name, shndx, res.raw_value, ValueFlags::empty())
        .with_context(|| {
            format!(
                "Failed to copy dynamic {}",
                layout.symbol_debug(sym_def.symbol_id)
            )
        })?;
    Ok(())
}

pub(crate) fn write_canonical_plt_dynamic_symbol_definition<'data, C: ElfClass>(
    sym_def: &crate::layout::DynamicSymbolDefinition<elf::Elf<C>>,
    object: &DynamicLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
    dynamic_symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
) -> Result {
    let sym_index = sym_def.symbol_id.to_input(object.symbol_id_range);
    let sym = object.object.symbol(sym_index)?;

    let resolution = layout
        .local_symbol_resolution(sym_def.symbol_id)
        .context("Canonical PLT symbol has no resolution")?;

    let entry = dynamic_symbol_writer.undefined_symbol(false, sym_def.name)?;
    entry.set_value(resolution.plt_address()?)?;
    entry.set_binding_and_type(sym.st_bind(), object::elf::STT_FUNC);

    Ok(())
}

pub(crate) fn write_regular_object_dynamic_symbol_definition<'data, C: ElfClass>(
    sym_def: &crate::layout::DynamicSymbolDefinition<elf::Elf<C>>,
    object: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
    dynamic_symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
) -> Result {
    let sym_index = sym_def.symbol_id.to_input(object.symbol_id_range);
    let sym = object.object.symbol(sym_index)?;
    let name = sym_def.name;
    let section_index = object.object.symbol_section(sym, sym_index)?;
    if section_index.is_none()
        && !platform::Symbol::is_common(sym)
        && !platform::Symbol::is_absolute(sym)
    {
        dynamic_symbol_writer
            .copy_symbol_shndx(sym, name, 0, 0, ValueFlags::empty())
            .with_context(|| {
                format!(
                    "Failed to copy dynamic {}",
                    layout.symbol_debug(sym_def.symbol_id)
                )
            })?;
        return Ok(());
    }

    let symbol_id = sym_def.symbol_id;
    let resolution = layout.local_symbol_resolution(symbol_id).with_context(|| {
        format!(
            "Tried to write dynamic symbol definition without a resolution: {}",
            layout.symbol_debug(symbol_id)
        )
    })?;

    // For non-PIE executables, export IFUNC symbols as STT_FUNC pointing to PLT stub.
    // For PIE executables, keep IFUNC as-is.
    if section_index.is_some()
        && resolution.flags.is_ifunc()
        && layout.symbol_db.output_kind.is_executable()
        && !layout.symbol_db.output_kind.is_position_independent()
        && let Some(plt_address) = resolution.format_specific.plt_address
    {
        let plt_output_section_id = layout
            .output_sections
            .primary_output_section(output_section_id::PLT_GOT);
        let shndx = dynamic_symbol_writer
            .output_sections
            .output_index_of_section(plt_output_section_id)
            .with_context(|| {
                format!(
                    "PLT section not found for ifunc symbol `{}`",
                    String::from_utf8_lossy(name),
                )
            })?;
        let size = object_symbol_size(sym, sym_index, object)?;
        let entry = dynamic_symbol_writer.define_symbol(
            false,
            SymbolSection::Index(shndx),
            plt_address.into(),
            size,
            Some(name),
        )?;
        entry.set_binding_and_type(sym.st_bind(), object::elf::STT_FUNC);
        entry.set_other(sym.st_other());
    } else {
        let mut symbol_value = resolution.value_for_symbol_table();
        if sym.st_type() == object::elf::STT_TLS {
            symbol_value -= layout.tls_start_address();
        }

        dynamic_symbol_writer
            .copy_object_symbol(
                sym,
                sym_index,
                symbol_id,
                name,
                object,
                layout,
                symbol_value,
                ValueFlags::empty(),
            )
            .with_context(|| {
                format!("Failed to copy dynamic {}", layout.symbol_debug(symbol_id))
            })?;
    }
    Ok(())
}
