use super::*;
use crate::bail;
use crate::elf::get_page_mask;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::layout::ObjectLayout;
use crate::layout::Resolution;
use crate::layout::Section;
use crate::macho::GOT_ENTRY_SIZE;
use crate::macho::MachO;
use crate::macho::PLT_ENTRY_SIZE;
use crate::macho::SectionFlags;
use crate::macho::output_section_id;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::HexU64;
use crate::platform::Arch;
use crate::platform::Args;
use crate::platform::ObjectFile as _;
use crate::platform::Relaxation as _;
use crate::resolution::SectionSlot;
use crate::symbol_db::SymbolId;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use linker_utils::elf::RelocationKind;
use object::SymbolIndex;
use object::macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12;
use object::macho::RelocationInfo;
use object::macho::S_THREAD_LOCAL_REGULAR;
use object::macho::S_THREAD_LOCAL_VARIABLES;
use object::macho::S_THREAD_LOCAL_ZEROFILL;
use std::ops::BitAnd;
use tracing::debug_span;

pub(crate) fn write_got_entries(layout: &MachOLayout<'_>, got: &mut [u8]) -> Result {
    let got_layout = layout.section_layouts.get(output_section_id::GOT);

    let sorted_symbols = &layout.format_specific.imported_symbols;
    for (i, imported_symbol) in sorted_symbols.iter().enumerate() {
        let offset = imported_symbol
            .got_address
            .get()
            .checked_sub(got_layout.mem_offset)
            .ok_or_else(|| error!("GOT entry address is before __got"))?
            as usize;
        let end = offset + GOT_ENTRY_SIZE as usize;

        /* DYLD_CHAINED_PTR_64 format:
        uint64_t dyld_chained_ptr_64_bind:
          ordinal: 24
          addend: 8 // 0 thru 255
          reserved: 19 // all zeros
          next: 12 // 4-byte stride
          bind: 1 // == 1
        */
        let bind = 1u64 << 63;
        // TODO: when crossing a page boundary, next is equal to zero
        let next = if i == sorted_symbols.len() - 1 { 0 } else { 2 };
        let next = next << 51;
        let ordinal = i as u64;
        got[offset..end].copy_from_slice(&(bind | next | ordinal).to_le_bytes());
    }

    Ok(())
}

pub(crate) fn write_plt_entries<A: Arch<Platform = MachO>>(
    layout: &MachOLayout<'_>,
    plt: &mut [u8],
) -> Result {
    let plt_layout = layout.section_layouts.get(output_section_id::PLT_GOT);

    for imported_symbol in &layout.format_specific.imported_symbols {
        let Some(stub_address) = imported_symbol.plt_address else {
            continue;
        };

        let offset = stub_address
            .get()
            .checked_sub(plt_layout.mem_offset)
            .ok_or_else(|| error!("STUB entry address is before __stubs"))?
            as usize;
        let end = offset + PLT_ENTRY_SIZE as usize;

        A::write_plt_entry(
            &mut plt[offset..end],
            imported_symbol.got_address.get(),
            stub_address.get(),
        )?;
    }

    Ok(())
}

pub(crate) fn write_object<'data, A: Arch<Platform = MachO>>(
    object: &ObjectLayout<'data, MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    symbol_writer: &mut MachOSymbolTableWriter,
) -> Result {
    verbose_timing_phase!("Write object", file_id = object.file_id.as_u32());

    let _span = debug_span!("write_file", filename = %object.input).entered();
    let _file_span = layout.args().common().trace_span_for_file(object.file_id);
    for (i, sec) in object.sections.iter().enumerate() {
        match sec {
            SectionSlot::Loaded(sec) => {
                write_object_section::<A>(object, layout, *sec, object::SectionIndex(i), buffers)?;
            }
            _ => (),
        }
    }

    write_symbols(object, buffers, layout, symbol_writer)?;

    Ok(())
}

pub(crate) fn write_object_section<'data, A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout<'data>,
    section: Section,
    section_index: object::SectionIndex,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) -> Result {
    let out = write_section_raw(object_layout, layout, section, section_index, buffers)?;

    let section_address = object_layout.section_resolutions[section_index.0]
        .address()
        .context("Attempted to apply relocations to a section that we didn't load")?;

    let section_flags = object_layout.object.section(section_index)?.flags.get(LE);

    for rel in object_layout.relocations(section_index)?.relocations {
        apply_relocation::<A>(
            object_layout,
            section_address,
            section_flags,
            rel.info(LE),
            layout,
            out,
        )?;
    }

    Ok(())
}

#[inline(always)]
pub(crate) fn apply_relocation<'data, A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'data, MachO>,
    section_address: u64,
    section_flags: SectionFlags,
    rel: RelocationInfo,
    layout: &MachOLayout<'data>,
    out: &mut [u8],
) -> Result {
    let mut offset_in_section = u64::from(rel.r_address);
    let place = section_address + offset_in_section;

    let _span = tracing::trace_span!(
        "relocation",
        address = place,
        address_hex = %HexU64::new(place)
    )
    .entered();

    let (resolution, _symbol_index, local_symbol_id) = get_resolution(rel, object_layout, layout)?;
    let flags = layout.flags_for_symbol(local_symbol_id);
    let output_kind = layout.symbol_db.output_kind;

    // TODO: We don't support addends, relaxation deltas, or previous relocations yet.
    let relaxation = A::new_relaxation(
        rel,
        out,
        offset_in_section,
        flags,
        output_kind,
        section_flags,
        None,
        resolution.raw_value,
        section_address,
        0,
        None,
    );

    let rel_info = match relaxation.as_ref() {
        Some(relaxation) => {
            relaxation.apply(out, &mut offset_in_section, &mut 0);
            relaxation.rel_info()
        }
        None if rel.r_type == ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => {
            bail!(
                "TLV relocations are currently only supported for locally-defined, strong, and \
                non-interposable symbols in executables"
            )
        }
        None => A::relocation_from_raw(rel)?,
    };

    let mask = get_page_mask(rel_info.mask);
    let value = match rel_info.kind {
        RelocationKind::Absolute
            if section_flags.typ() == S_THREAD_LOCAL_VARIABLES
                && flags.has_link_time_address()
                && is_tlv_template_referent(layout, local_symbol_id) =>
        {
            // TODO: Once addends are supported, remember to change this to S + A -
            // tlv_data_start_address().
            resolution
                .raw_value
                .wrapping_sub(layout.tlv_data_start_address())
        }
        RelocationKind::Absolute => resolution.raw_value.bitand(mask.symbol_plus_addend),
        RelocationKind::AbsoluteLowPart => resolution.raw_value.bitand(mask.symbol_plus_addend),
        RelocationKind::Relative => resolution
            .raw_value
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::GotRelative => resolution
            .raw_value
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::Got => resolution.raw_value.bitand(mask.symbol_plus_addend),
        _ => todo!(),
    };

    tracing::trace!(
            %flags,
            ?rel_info.kind,
            %rel_info.size,
            value,
            value_hex = %HexU64::new(value),
            symbol_name = %layout.symbol_db.symbol_name_for_display(local_symbol_id),
            "relocation applied");

    rel_info
        .write_to_buffer(value, &mut out[offset_in_section as usize..])
        .with_context(|| {
            format!(
                "Failed to apply relocation {} to {}",
                A::rel_type_to_string(rel),
                layout.symbol_debug(local_symbol_id)
            )
        })?;

    Ok(())
}

fn is_tlv_template_referent(layout: &MachOLayout<'_>, symbol_id: SymbolId) -> bool {
    layout
        .symbol_db
        .output_section_id(layout.symbol_db.definition(symbol_id))
        .map(|id| layout.output_sections.primary_output_section(id))
        .is_some_and(|id| {
            matches!(
                layout.output_sections.section_flags(id).typ(),
                S_THREAD_LOCAL_REGULAR | S_THREAD_LOCAL_ZEROFILL
            )
        })
}

pub(crate) fn write_section_raw<'out, 'data>(
    object: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout,
    sec: Section,
    section_index: object::SectionIndex,
    buffers: &'out mut OutputSectionPartMap<&mut [u8]>,
) -> Result<&'out mut [u8]> {
    let part_id = object.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    if layout
        .output_sections
        .has_data_in_file(part_id.output_section_id::<MachO>())
    {
        let section_buffer = buffers.get_mut(part_id);
        let allocation_size = sec.capacity(part_id, &layout.output_sections) as usize;
        if section_buffer.len() < allocation_size {
            bail!(
                "Insufficient space allocated to section `{}`. Tried to take {} bytes, but only {} remain",
                object.object.section_display_name(section_index),
                allocation_size,
                section_buffer.len()
            );
        }
        let out = section_buffer.split_off_mut(..allocation_size).unwrap();
        let object_section = object.object.section(section_index)?;

        let section_size = object.object.section_size(object_section)?;
        let (out, padding) = out.split_at_mut(section_size as usize);
        object.object.copy_section_data(object_section, out)?;
        padding.fill(0);
        Ok(out)
    } else {
        Ok(&mut [])
    }
}

pub(crate) fn get_resolution<'data>(
    rel: RelocationInfo,
    object_layout: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout,
) -> Result<(Resolution<MachO>, SymbolIndex, SymbolId)> {
    let symbol_index = SymbolIndex(rel.r_symbolnum as usize);
    let local_symbol_id = object_layout.symbol_id_range.input_to_id(symbol_index);
    let sym = object_layout.object.symbol(symbol_index)?;
    let section_index = object_layout.object.symbol_section(sym, symbol_index)?;
    let resolution = layout
        .merged_symbol_resolution(local_symbol_id)
        .or_else(|| {
            section_index.and_then(|section_index| {
                let section_address =
                    object_layout.section_resolutions[section_index.0].address()?;
                Some(Resolution {
                    raw_value: section_address,
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
