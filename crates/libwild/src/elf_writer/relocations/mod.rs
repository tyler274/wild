mod apply;
mod eh_frame;
mod rela;

use self::elf::get_page_mask;
use super::types::*;
use crate::bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::FileLayout;
use crate::layout::Layout;
use crate::layout::ObjectLayout;
use crate::layout::Resolution;
use crate::part_id::PartId;
use crate::platform;
use crate::platform::Arch;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::Relocation;
use crate::platform::SectionFlags as _;
use crate::resolution::SectionSlot;
use crate::string_merging::get_merged_string_output_address;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::thunks::ThunkBlockId;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
#[allow(unused_imports)]
pub(crate) use apply::*;
#[allow(unused_imports)]
pub(crate) use eh_frame::*;
use linker_utils::elf::DynamicRelocationKind;
use linker_utils::elf::RelocationKind;
use linker_utils::elf::RelocationKindInfo;
use linker_utils::elf::RelocationSize;
use linker_utils::elf::SectionFlags;
use linker_utils::relaxation::opt_input_to_output;
use object::SymbolIndex;
use object::read::elf::Sym as _;
#[allow(unused_imports)]
pub(crate) use rela::*;
use std::fmt::Display;
use std::marker::PhantomData;
use std::ops::BitAnd;

pub(crate) fn display_relocation<
    'a,
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation,
>(
    object: &'a ObjectLayout<'data, elf::Elf<C>>,
    rel: &'a R,
    layout: &'a ElfLayout<'data, C>,
) -> DisplayRelocation<'a, 'data, C, A, R> {
    DisplayRelocation::<'a, 'data, C, A, R> {
        rel,
        symbol_db: &layout.symbol_db,
        per_symbol_flags: &layout.per_symbol_flags,
        object,
        phantom: PhantomData,
    }
}

pub(crate) struct DisplayRelocation<
    'a,
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation,
> {
    pub(crate) rel: &'a R,
    pub(crate) symbol_db: &'a SymbolDb<'data, elf::Elf<C>>,
    pub(crate) per_symbol_flags: &'a PerSymbolFlags,
    pub(crate) object: &'a ObjectLayout<'data, elf::Elf<C>>,
    pub(crate) phantom: PhantomData<A>,
}

impl<'a, 'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>, R: Relocation<Platform = elf::Elf<C>>>
    Display for DisplayRelocation<'a, 'data, C, A, R>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "relocation of type {} to ",
            A::rel_type_to_string(self.rel.raw_type())
        )?;
        match self.rel.symbol() {
            None => write!(f, "absolute")?,
            Some(local_symbol_index) => {
                let symbol_id = self.object.symbol_id_range.input_to_id(local_symbol_index);
                write!(
                    f,
                    "{}",
                    self.symbol_db
                        .symbol_debug(self.per_symbol_flags, symbol_id)
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SectionInfo<S: platform::SectionFlags> {
    pub(crate) section_address: u64,
    pub(crate) is_writable: bool,
    pub(crate) section_flags: S,
    pub(crate) part_id: PartId,
}

pub(crate) fn reloc_file_offset<C: ElfClass, S: platform::SectionFlags>(
    layout: &ElfLayout<C>,
    section_info: SectionInfo<S>,
    offset_in_section: u64,
) -> u64 {
    let rec = layout.section_part_layouts.get(section_info.part_id);
    rec.file_offset as u64
        + section_info.section_address.wrapping_sub(rec.mem_offset)
        + offset_in_section
}

pub(crate) fn get_resolution<'data, C: ElfClass, R: Relocation>(
    rel: &R,
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
) -> Result<(Resolution<elf::Elf<C>>, SymbolIndex, SymbolId)> {
    let symbol_index = rel.symbol().context("Unsupported absolute relocation")?;
    let local_symbol_id = object_layout.symbol_id_range.input_to_id(symbol_index);
    let sym = object_layout.object.symbol(symbol_index)?;
    let section_index = object_layout.object.symbol_section(sym, symbol_index)?;
    let resolution = layout
        .merged_symbol_resolution(local_symbol_id)
        .or_else(|| {
            section_index.and_then(|section_index| {
                let section_address =
                    object_layout.section_resolutions[section_index.0].address()?;
                let output_offset = opt_input_to_output(
                    object_layout.section_relax_deltas.get(section_index.0),
                    crate::platform::Symbol::value(sym),
                );

                Some(Resolution {
                    raw_value: section_address + output_offset,
                    dynamic_symbol_index: None,
                    flags: ValueFlags::empty(),
                    format_specific: Default::default(),
                })
            })
        })
        .with_context(|| {
            format!(
                "Missing resolution for: {}",
                layout.symbol_debug(local_symbol_id)
            )
        })?;
    Ok((resolution, symbol_index, local_symbol_id))
}

/// Returns the `st_other` byte of the canonical definition of `symbol_id`, or 0 if it isn't
/// defined by a regular object. Used for ppc64 local-entry-point computation.
pub(crate) fn callee_st_other<C: ElfClass>(layout: &ElfLayout<C>, symbol_id: SymbolId) -> u8 {
    let canonical = layout.symbol_db.definition(symbol_id);
    let file_id = layout.symbol_db.file_id_for_symbol(canonical);
    if let FileLayout::Object(obj) = layout.file_layout(file_id)
        && let Ok(sym) = obj.object.symbol(canonical.to_input(obj.symbol_id_range))
    {
        return sym.st_other().0;
    }
    0
}

#[inline(always)]
pub(crate) fn get_pair_subtraction_relocation_value<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
>(
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    rel: &R,
    layout: &ElfLayout<C>,
    resolution: Resolution<elf::Elf<C>>,
    symbol_index: SymbolIndex,
    addend: i64,
    set_rel: &R,
    expected_r_type: object::elf::RelocationType,
) -> Result<u64> {
    ensure!(
        set_rel.offset() == rel.offset(),
        "PairSubtractionULEB128 relocation must have equal offset"
    );
    ensure!(
        set_rel.raw_type() == expected_r_type,
        "unexpected previous relocation: expected: {}, was: {}",
        A::rel_type_to_string(expected_r_type),
        A::rel_type_to_string(set_rel.raw_type())
    );
    let (set_resolution, set_symbol_index, _) = get_resolution(set_rel, object_layout, layout)?;

    let set_resolution_val = set_resolution.value_with_addend(
        set_rel.addend(),
        set_symbol_index,
        object_layout,
        &layout.symbol_db.section_part_ids,
        &layout.merged_strings,
        &layout.merged_string_start_addresses,
    )?;
    let sub_resolution_val = resolution.value_with_addend(
        addend,
        symbol_index,
        object_layout,
        &layout.symbol_db.section_part_ids,
        &layout.merged_strings,
        &layout.merged_string_start_addresses,
    )?;
    Ok(set_resolution_val.wrapping_sub(sub_resolution_val))
}

/// Checks if we need to use a thunk for a relocation and if we do, return the value to use for the
/// thunk.
pub(crate) fn maybe_get_thunk_for_relocation<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object_layout: &ObjectLayout<elf::Elf<C>>,
    section_info: SectionInfo<SectionFlags>,
    layout: &Layout<elf::Elf<C>>,
    rel_info: RelocationKindInfo,
    local_symbol_id: SymbolId,
    place: u64,
    value: u64,
) -> Result<Option<u64>> {
    let Some(config) = A::thunk_config() else {
        return Ok(None);
    };

    if !rel_info.thunkable {
        return Ok(None);
    }

    if rel_info.range.contains(value as i64) {
        return Ok(None);
    }

    let canonical_id = layout.symbol_db.definition(local_symbol_id);

    let thunk_id = if section_info.part_id == config.primary_function_part_id {
        object_layout.thunk_block_id
    } else {
        ThunkBlockId::FIRST
    };

    let thunk_address_opt = layout
        .thunk_block_addresses
        .get(thunk_id.as_usize())
        .and_then(|m| m.get(&canonical_id))
        .copied();

    if let Some(thunk_address) = thunk_address_opt {
        if thunk_address == 0 {
            bail!(
                "Thunk address not yet allocated for symbol {}",
                layout.symbol_db.symbol_name_for_display(local_symbol_id)
            );
        }

        let mask = get_page_mask(rel_info.mask);
        let new_value = thunk_address
            .wrapping_add(rel_info.bias)
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place));

        tracing::trace!(
            old_value = value,
            new_value,
            thunk_address,
            "Using thunk instead of out-of-range branch"
        );

        return Ok(Some(new_value));
    }

    bail!(
        "Branch relocation out of range by {over} for symbol {sym} \
         but no thunk allocated. Part: {part}. Offset: {offset}",
        over = rel_info.range.overrun(value as i64),
        sym = layout.symbol_db.symbol_name_for_display(local_symbol_id),
        part = layout.output_sections.part_debug(section_info.part_id),
        offset = value as i64,
    );
}

pub(crate) fn apply_debug_relocation<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
>(
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    offset_in_section: u64,
    rel: &R,
    layout: &ElfLayout<C>,
    section_tombstone_value: u64,
    out: &mut [u8],
    relocation_cache: &RelocationCache<R>,
) -> Result<()> {
    let symbol_index = rel.symbol().context("Unsupported absolute relocation")?;
    let sym = object_layout.object.symbol(symbol_index)?;
    let section_index = object_layout.object.symbol_section(sym, symbol_index)?;

    let addend = rel.addend();
    let r_type = rel.raw_type();
    let rel_info = A::relocation_from_raw(r_type)?;

    let resolution = layout
        .merged_symbol_resolution(object_layout.symbol_id_range.input_to_id(symbol_index))
        .or_else(|| {
            section_index.and_then(|section_index| {
                let section_address =
                    object_layout.section_resolutions[section_index.0].address()?;
                // Include the symbol's offset within the section (adjusted for any relaxation
                // deltas). This is necessary on architectures like RISC-V and LoongArch64 where
                // debug info references local symbols (e.g. .LFB0, .LFE0) whose value is their
                // offset within the section, rather than section symbols where the offset is
                // encoded in the relocation addend.
                let output_offset = opt_input_to_output(
                    object_layout.section_relax_deltas.get(section_index.0),
                    crate::platform::Symbol::value(sym),
                );

                Some(Resolution {
                    raw_value: section_address + output_offset,
                    dynamic_symbol_index: None,
                    flags: ValueFlags::empty(),
                    format_specific: Default::default(),
                })
            })
        });

    let value = if let Some(resolution) = resolution {
        match rel_info.kind {
            RelocationKind::Absolute
            | RelocationKind::AbsoluteSet
            | RelocationKind::AbsoluteSetWord6
            | RelocationKind::AbsoluteAddition
            | RelocationKind::AbsoluteAdditionWord6
            | RelocationKind::AbsoluteSubtraction
            | RelocationKind::AbsoluteSubtractionWord6 => {
                let mut value = resolution.value_with_addend(
                    addend,
                    symbol_index,
                    object_layout,
                    &layout.symbol_db.section_part_ids,
                    &layout.merged_strings,
                    &layout.merged_string_start_addresses,
                )?;
                // Adjust the relocation value based on the value at the place.
                if matches!(
                    rel_info.kind,
                    RelocationKind::AbsoluteAddition
                        | RelocationKind::AbsoluteSubtraction
                        | RelocationKind::AbsoluteSetWord6
                        | RelocationKind::AbsoluteSubtractionWord6
                ) {
                    value = rel_info.adjust_value_based_on_content(
                        value,
                        out,
                        offset_in_section as usize,
                    )?;
                }
                value
            }
            RelocationKind::DtpOff => resolution
                .value()
                .wrapping_sub(layout.tls_end_address())
                .wrapping_add(addend as u64),
            RelocationKind::PairSubtractionULEB128(expected_r_type) => {
                get_pair_subtraction_relocation_value::<C, A, R>(
                    object_layout,
                    rel,
                    layout,
                    resolution,
                    symbol_index,
                    addend,
                    // Must be the previous relocation.
                    &relocation_cache.previous.with_context(|| {
                        "Missing previous relocation for PairSubtractionULEB128".to_owned()
                    })?,
                    expected_r_type,
                )?
            }
            // Skip R_RISCV_SET_ULEB128
            RelocationKind::Relative if rel_info.size == RelocationSize::ByteSize(0) => 0,
            kind => bail!("Unsupported debug relocation kind {kind:?}"),
        }
    } else if let Some(section_index) = section_index {
        match object_layout.sections[section_index.0] {
            SectionSlot::MergeStrings(..) => get_merged_string_output_address::<elf::Elf<C>>(
                symbol_index,
                addend,
                object_layout.object,
                &object_layout.sections,
                &layout.symbol_db.section_part_ids,
                object_layout.section_id_range,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
                false,
            )?
            .context("Cannot get merged string offset for a debug info section")?,
            SectionSlot::Discard | SectionSlot::Unloaded(..) => section_tombstone_value,
            _ => bail!("Could not find a relocation resolution for a debug info section"),
        }
    } else {
        // Debug info can sometimes contain relocations for symbols from other objects. If we didn't
        // load those symbols, then we need to use the tombstone value. Careful, we don't have any
        // tests for this, but building chromium does trigger this branch.
        section_tombstone_value
    };

    rel_info.write_to_buffer(value, &mut out[offset_in_section as usize..])?;

    Ok(())
}

#[inline(always)]
pub(crate) fn write_absolute_relocation<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    table_writer: &mut TableWriter<'_, '_, C>,
    resolution: Resolution<elf::Elf<C>>,
    place: u64,
    addend: i64,
    section_info: SectionInfo<<A::Platform as Platform>::SectionFlags>,
    symbol_index: object::SymbolIndex,
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
) -> Result<u64> {
    if !section_info.section_flags.is_alloc() {
        resolution.value_with_addend(
            addend,
            symbol_index,
            object_layout,
            &layout.symbol_db.section_part_ids,
            &layout.merged_strings,
            &layout.merged_string_start_addresses,
        )
    } else if resolution.flags.is_dynamic()
        && resolution.flags.is_absolute()
        && !section_info.is_writable
    {
        // Weak undefined symbol referenced from a read-only section. Fill in as zero.
        Ok(0)
    } else if resolution.flags.is_interposable() && section_info.is_writable {
        table_writer.write_dynamic_symbol_relocation::<A>(
            place,
            addend,
            resolution.dynamic_symbol_index()?,
            DynamicRelocationKind::Absolute,
        )?;

        Ok(0)
    } else if resolution.flags.is_ifunc()
        && section_info.is_writable
        && table_writer.output_kind.is_position_independent()
    {
        table_writer
            .write_ifunc_relocation_for_data::<A>(place, resolution.raw_value as i64 + addend)?;
        Ok(0)
    } else if table_writer.output_kind.is_position_independent() && !resolution.is_absolute() {
        let address = resolution.value_with_addend(
            addend,
            symbol_index,
            object_layout,
            &layout.symbol_db.section_part_ids,
            &layout.merged_strings,
            &layout.merged_string_start_addresses,
        )?;
        table_writer.write_address_relocation::<A>(place, address)
    } else {
        resolution.value_with_addend(
            addend,
            symbol_index,
            object_layout,
            &layout.symbol_db.section_part_ids,
            &layout.merged_strings,
            &layout.merged_string_start_addresses,
        )
    }
}
