use self::elf::get_page_mask;
use super::super::types::*;
use super::*;
use crate::OutputKind;
use crate::bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::ObjectLayout;
use crate::output_trace::HexU64;
use crate::output_trace::TraceOutput;
use crate::platform::Arch;
use crate::platform::PreviousRelocationInfo;
use crate::platform::Relaxation as _;
use crate::platform::Relocation;
use linker_utils::elf::RelocationKind;
use linker_utils::loongarch64::highest_relocation_with_bias;
use linker_utils::relaxation::RelocationModifier;
use linker_utils::relaxation::SectionRelaxDeltas;
use linker_utils::relaxation::opt_input_to_output;
use std::ops::BitAnd;
use std::ops::Sub;

/// Applies the relocation `rel` at `offset_in_section`, where the section bytes are `out`. See "ELF
/// Handling For Thread-Local Storage" for details about some of the TLS-related relocations and
/// transformations that are applied.
#[inline(always)]
pub(crate) fn apply_relocation<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
    I: Iterator<Item = object::Result<R>> + Clone,
>(
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    mut offset_in_section: u64,
    rel: &R,
    section_info: SectionInfo<linker_utils::elf::SectionFlags>,
    layout: &ElfLayout<'data, C>,
    out: &mut [u8],
    table_writer: &mut TableWriter<'_, '_, C>,
    trace: &TraceOutput,
    relocation_cache: &RelocationCache<R>,
    relocation_iterator: &I,
    relax_deltas: Option<&SectionRelaxDeltas>,
) -> Result<RelocationModifier> {
    let section_address = section_info.section_address;
    let original_place = section_address + offset_in_section;
    let _span = tracing::trace_span!(
        "relocation",
        address = original_place,
        address_hex = %HexU64::new(original_place)
    )
    .entered();

    let r_type = rel.raw_type();
    let mut addend = rel.addend();

    match A::relocation_from_raw(r_type)?.kind {
        RelocationKind::None => return Ok(RelocationModifier::Normal),
        RelocationKind::Alignment => {
            let addend = addend as u64;
            let removed_bytes =
                relax_deltas.map_or(0u64, |d| u64::from(d.delta_bytes_at(rel.offset())));
            let padding_bytes = addend.saturating_sub(removed_bytes) as usize;
            let offset_in_section = offset_in_section as usize;
            A::fill_nop_padding(&mut out[offset_in_section..offset_in_section + padding_bytes]);

            return Ok(RelocationModifier::Normal);
        }
        RelocationKind::Relative if rel.symbol().is_none() => {
            if layout.symbol_db.output_kind.is_position_independent() {
                bail!(
                    "relocation of type {} to absolute address cannot be used in \
                    position-independent output; recompile with -fPIC",
                    rel.raw_type()
                );
            }
            let place = section_info.section_address + offset_in_section;
            let value = (rel.addend() as u64).wrapping_sub(place);
            A::relocation_from_raw(r_type)?
                .write_to_buffer(value, &mut out[offset_in_section as usize..])?;
            return Ok(RelocationModifier::Normal);
        }
        RelocationKind::Absolute
            if layout.symbol_db.output_kind.is_shared_object()
                && A::is_disallowed_in_shared_object(r_type) =>
        {
            bail!(
                "relocation type {} cannot be used when making a shared object; \
                 recompile with -fPIC",
                A::rel_type_to_string(r_type)
            );
        }
        _ => {}
    }
    let (resolution, symbol_index, local_symbol_id) = get_resolution(rel, object_layout, layout)?;
    let flags = layout.flags_for_symbol(local_symbol_id);
    if layout.symbol_db.output_kind.is_position_independent()
        && (flags.is_interposable() || flags.is_dynamic())
        && !flags.needs_copy_relocation()
        && !flags.needs_plt()
        && A::is_disallowed_for_interposable_symbols(r_type)
    {
        bail!(
            "relocation {} cannot be used against symbol; recompile with -fPIC",
            A::rel_type_to_string(r_type)
        );
    }
    let mut next_modifier = RelocationModifier::Normal;
    let rel_info;
    let output_kind = layout.symbol_db.output_kind;

    let relaxation = A::new_relaxation(
        r_type,
        out,
        offset_in_section,
        flags,
        output_kind,
        section_info.section_flags,
        relax_deltas,
        resolution.raw_value,
        section_address,
        rel.addend(),
        relocation_cache
            .previous
            .as_ref()
            .filter(|r| r.symbol() == rel.symbol())
            .map(|r| PreviousRelocationInfo {
                kind: r.raw_type(),
                offset: r.offset(),
                symbol: r.symbol(),
                addend: r.addend(),
            }),
    )
    .filter(|relaxation| layout.args().relax || relaxation.is_mandatory());

    if let Some(relaxation) = &relaxation {
        rel_info = relaxation.rel_info();
        relaxation.apply(out, &mut offset_in_section, &mut addend);
        next_modifier = relaxation.next_modifier();
    } else {
        rel_info = A::relocation_from_raw(r_type)?;
    }

    // Compute place to which IP-relative relocations will be relative. This is different to
    // `original_place` in that our `offset_in_section` may have been adjusted by a relaxation.
    let place = section_address + offset_in_section;

    let mask = get_page_mask(rel_info.mask);
    let bias = rel_info.bias;
    // For ppc64 calls, branch to the callee's local entry point (we share its TOC, so the global
    // entry's r2 setup is unnecessary). Zero for every other architecture and relocation.
    let branch_local_entry = if rel_info.size.is_ppc64_branch() {
        A::local_entry_offset(callee_st_other(layout, local_symbol_id))
    } else {
        0
    };
    let mut value = match rel_info.kind {
        RelocationKind::Absolute => write_absolute_relocation::<C, A>(
            table_writer,
            resolution,
            place,
            addend,
            section_info,
            symbol_index,
            object_layout,
            layout,
        )?,
        RelocationKind::AbsoluteSet
        | RelocationKind::AbsoluteSetWord6
        | RelocationKind::AbsoluteAddition
        | RelocationKind::AbsoluteAdditionWord6
        | RelocationKind::AbsoluteSubtraction
        | RelocationKind::AbsoluteSubtractionWord6 => resolution.value_with_addend(
            addend,
            symbol_index,
            object_layout,
            &layout.symbol_db.section_part_ids,
            &layout.merged_strings,
            &layout.merged_string_start_addresses,
        )?,
        RelocationKind::AbsoluteLowPart => resolution
            .value_with_addend(
                addend,
                symbol_index,
                object_layout,
                &layout.symbol_db.section_part_ids,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
            )?
            .bitand(mask.symbol_plus_addend),
        RelocationKind::Relative => resolution
            .value_with_addend(
                addend,
                symbol_index,
                object_layout,
                &layout.symbol_db.section_part_ids,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
            )?
            .wrapping_add(branch_local_entry)
            .wrapping_add(bias)
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::RelativeLoongArchHigh => highest_relocation_with_bias(
            resolution.value_with_addend(
                addend,
                symbol_index,
                object_layout,
                &layout.symbol_db.section_part_ids,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
            )?,
            place,
        ),
        RelocationKind::RelativeRiscVLow12 => {
            // The iterator is used for e.g. R_RISCV_PCREL_HI20 & R_RISCV_PCREL_LO12_I pair of
            // relocations where the later one actually points to a label of the HI20
            // relocations and thus we need to find it. The relocation is typically
            // right before the LO12_* relocation.
            ensure!(
                addend == 0,
                "Unexpected addend for R_RISCV_PCREL_LO12 relocation"
            );
            let hi_offset_in_section = resolution
                .value_with_addend(
                    addend,
                    symbol_index,
                    object_layout,
                    &layout.symbol_db.section_part_ids,
                    &layout.merged_strings,
                    &layout.merged_string_start_addresses,
                )?
                .wrapping_sub(section_address);
            let hi_rel = relocation_cache
                .high_part_symbols
                .get(&hi_offset_in_section)
                .copied()
                .or_else(|| {
                    // It's very unlikely that a high part follows the low part:
                    relocation_iterator.clone().find_map(|r| {
                        if let Ok(r) = r
                            && A::high_part_relocations().contains(&r.raw_type())
                        {
                            let r_output_offset = opt_input_to_output(relax_deltas, r.offset());
                            if r_output_offset == hi_offset_in_section {
                                return Some(r);
                            }
                        }
                        None
                    })
                })
                .context("Missing High relocation connected with R_RISCV_PCREL_LO12")?;

            let hi_rel_info = A::relocation_from_raw(hi_rel.raw_type())?;
            let addend = hi_rel.addend();
            let (resolution, symbol_index, _) = get_resolution(&hi_rel, object_layout, layout)
                .with_context(|| {
                    "Missing High resolution connected to R_RISCV_PCREL_LO12".to_string()
                })?;
            let place = section_address + hi_offset_in_section;

            // Only a subset of relocations is referenced by R_RISCV_PCREL_LO12 relocations.
            match hi_rel_info.kind {
                RelocationKind::Relative => resolution
                    .value_with_addend(
                        addend,
                        symbol_index,
                        object_layout,
                        &layout.symbol_db.section_part_ids
                            [object_layout.section_id_range.as_usize()],
                        &layout.merged_strings,
                        &layout.merged_string_start_addresses,
                    )?
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                RelocationKind::GotRelative => resolution
                    .got_address_for_relocation()?
                    .wrapping_add(addend as u64)
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                RelocationKind::TlsGd => resolution
                    .tlsgd_got_address()?
                    .wrapping_add(addend as u64)
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                RelocationKind::TlsLd => layout
                    .prelude()
                    .format_specific
                    .tlsld_got_entry
                    .unwrap()
                    .get()
                    .wrapping_add(addend as u64)
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                RelocationKind::GotTpOff => resolution
                    .got_address()?
                    .wrapping_add(addend as u64)
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                _ => bail!(
                    "Unsupported high part relocation {:?} connected with R_RISCV_PCREL_LO12",
                    hi_rel_info.kind
                ),
            }
        }
        RelocationKind::PairSubtractionULEB128(expected_r_type) => {
            get_pair_subtraction_relocation_value::<C, A, R>(
                object_layout,
                rel,
                layout,
                resolution,
                symbol_index,
                addend,
                // It must be the previous relocation
                &relocation_cache.previous.with_context(|| {
                    "Missing previous relocation for PairSubtractionULEB128".to_owned()
                })?,
                expected_r_type,
            )?
        }
        RelocationKind::GotRelative => resolution
            .got_address_for_relocation()?
            .wrapping_add(bias)
            .wrapping_add(addend as u64)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::GotRelativeLoongArch64 => highest_relocation_with_bias(
            resolution
                .got_address_for_relocation()?
                .wrapping_add(addend as u64),
            place,
        ),
        RelocationKind::GotRelGotBase => resolution
            .got_address_for_relocation()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::Got => {
            // The LoongArch64 psABI does not provide a separate GOT Low part relocation for the
            // TLSGD relocation. So we need to distinguish between a classical GOT
            // slot and one corresponding to TLSGD.
            //
            // Note: TLSLD is unsupported by the target (https://github.com/loongson/la-abi-specs/issues/19).
            if resolution.flags.needs_got_tls_module() {
                resolution.tlsgd_got_address()?
            } else {
                resolution.got_address_for_relocation()?
            }
            .wrapping_add(bias)
            .bitand(mask.got_entry)
        }
        RelocationKind::SymRelGotBase => resolution
            .value_with_addend(
                addend,
                symbol_index,
                object_layout,
                &layout.symbol_db.section_part_ids,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
            )?
            .wrapping_add(bias)
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::PltRelGotBase => resolution
            .plt_address()?
            .wrapping_add(bias)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::PltRelative => resolution
            .plt_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .wrapping_sub(place.bitand(mask.place)),
        // TLS-related relocations
        RelocationKind::TlsGd => resolution
            .tlsgd_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::TlsGdGot => resolution
            .tlsgd_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry),
        RelocationKind::TlsGdGotBase => resolution
            .tlsgd_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::TlsLd => layout
            .prelude()
            .format_specific
            .tlsld_got_entry
            .unwrap()
            .get()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::TlsLdGot => layout
            .prelude()
            .format_specific
            .tlsld_got_entry
            .unwrap()
            .get()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry),
        RelocationKind::TlsLdGotBase => layout
            .prelude()
            .format_specific
            .tlsld_got_entry
            .unwrap()
            .get()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::DtpOff if output_kind == OutputKind::SharedObject => resolution
            .value()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .sub(layout.tls_start_address()),
        RelocationKind::DtpOff => resolution
            .value()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .wrapping_sub(layout.tls_end_address()),
        RelocationKind::GotTpOff => resolution
            .got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::GotTpOffLoongArch64 => highest_relocation_with_bias(
            resolution.got_address()?.wrapping_add(addend as u64),
            place,
        ),
        RelocationKind::GotTpOffGot => resolution
            .got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry),
        RelocationKind::GotTpOffGotBase => resolution
            .got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::TpOff
            if layout
                .symbol_db
                .is_undefined(layout.symbol_db.definition(local_symbol_id)) =>
        {
            // An undefined weak TLS symbol has no offset within the TLS block, so we somewhat
            // arbitrarily give the 0 offset which at least some other linkers also do and most
            // importantly is a value guaranteed to fit within the range of any relocation.
            (addend as u64).wrapping_add(bias)
        }
        RelocationKind::TpOff => resolution
            .value()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .wrapping_sub(A::tp_offset_start(layout)),
        RelocationKind::TlsDesc => resolution
            .tls_descriptor_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::TlsDescLoongArch64 => highest_relocation_with_bias(
            resolution
                .tls_descriptor_got_address()?
                .wrapping_add(addend as u64),
            place,
        ),
        RelocationKind::TlsDescGot => resolution
            .tls_descriptor_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry),
        RelocationKind::TlsDescGotBase => resolution
            .tls_descriptor_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::None | RelocationKind::TlsDescCall => 0,
        RelocationKind::Alignment => unreachable!(),
    };

    let offset_in_section = offset_in_section as usize;

    // Handle addition and subtraction relocation kinds.
    if matches!(
        rel_info.kind,
        RelocationKind::AbsoluteAddition
            | RelocationKind::AbsoluteAdditionWord6
            | RelocationKind::AbsoluteSubtraction
            | RelocationKind::AbsoluteSetWord6
            | RelocationKind::AbsoluteSubtractionWord6
    ) {
        value = rel_info.adjust_value_based_on_content(value, out, offset_in_section)?;
    }

    if let Some(relaxation) = relaxation {
        trace.emit(original_place, || {
            format!(
                "relaxation applied relaxation={kind:?}, flags={flags},\n\
                rel_kind={rel_kind:?},\n\
                value=0x{value:x}, symbol_name={symbol_name}",
                kind = relaxation.debug_kind(),
                rel_kind = rel_info.kind,
                symbol_name = layout.symbol_db.symbol_name_for_display(local_symbol_id),
            )
        });
        tracing::trace!(
            %flags,
            relaxation_kind = ?relaxation.debug_kind(),
            ?rel_info.kind,
            %rel_info.size,
            value,
            value_hex = %HexU64::new(value),
            symbol_name = %layout.symbol_db.symbol_name_for_display(local_symbol_id),
            "relaxation applied");
    } else {
        trace.emit(original_place, || {
            format!(
                "relocation applied flags={flags},\n\
                rel_kind={rel_kind:?},\n\
                value=0x{value:x}, symbol_name={symbol_name}",
                rel_kind = rel_info.kind,
                symbol_name = layout.symbol_db.symbol_name_for_display(local_symbol_id),
            )
        });
        tracing::trace!(
            %flags,
            ?rel_info.kind,
            %rel_info.size,
            value,
            value_hex = %HexU64::new(value),
            symbol_name = %layout.symbol_db.symbol_name_for_display(local_symbol_id),
            "relocation applied");
    }

    if let Some(thunked_value) = maybe_get_thunk_for_relocation::<C, A>(
        object_layout,
        section_info,
        layout,
        rel_info,
        local_symbol_id,
        place,
        value,
    )? {
        value = thunked_value;
    }

    rel_info.write_to_buffer(value, &mut out[offset_in_section..])?;

    Ok(next_modifier)
}
