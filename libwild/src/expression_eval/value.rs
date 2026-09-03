use super::*;
use crate::bail;
use crate::error::Context;
use crate::error::Result;
use crate::input_data::InputRef;
use crate::layout;
use crate::layout::OutputRecordLayout;
use crate::linker_script::Expression;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_id::SectionName;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::SymbolLoc;
use crate::part_id::PartId;
use crate::platform::Args;
use crate::platform::Platform;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use hashbrown::HashMap;

/// Compute 1-based line number by counting newlines before `remainder` in `file_bytes`.
fn line_number(file_bytes: &[u8], remainder: &[u8]) -> u32 {
    let parsed_len = file_bytes.len().saturating_sub(remainder.len());
    let consumed = &file_bytes[..parsed_len];
    consumed.iter().filter(|&&b| b == b'\n').count() as u32 + 1
}

#[derive(Clone, Default, Debug)]
pub(crate) struct ResolvedLocationCounter {
    pub(crate) value: u64,
    pub(crate) section_offset: Option<u64>,
}

pub(crate) enum SymbolValue {
    Absolute(u64),
    PartRelative {
        part_id: PartId,
        offset: u64,
    },
    SectionRelative {
        section_id: OutputSectionId,
        address: u64,
    },
}

fn evaluate_symbol_value<P: Platform>(
    symbol_value: &SymbolValue,
    loc: &SymbolLoc,
    output_sections: &OutputSections<'_, P>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    laid_out_mem_offsets: &OutputSectionPartMap<Option<u64>>,
    value_kind: &mut ExpressionValueKind,
    name: &[u8],
) -> Result<u64> {
    let current_section = loc
        .relative_section_id()
        .map(|id| output_sections.primary_output_section(id));
    match symbol_value {
        SymbolValue::Absolute(value) => {
            value_kind.contains_absolute = true;
            Ok(*value)
        }
        SymbolValue::SectionRelative {
            section_id,
            address,
        } => {
            let primary_section = output_sections.primary_output_section(*section_id);
            if current_section == Some(primary_section) {
                value_kind.contains_section_relative = true;
                let section_base = section_layouts.get(primary_section).mem_offset;
                let offset = address.checked_sub(section_base).with_context(|| {
                    format!(
                        "address of symbol '{}' is before its output section",
                        String::from_utf8_lossy(name)
                    )
                })?;
                Ok(offset)
            } else if let Some(section) = current_section {
                value_kind.contains_absolute = true;
                let section_base = section_layouts.get(section).mem_offset;
                Ok(address + section_base)
            } else {
                value_kind.contains_absolute = true;
                Ok(*address)
            }
        }
        SymbolValue::PartRelative { part_id, offset } => {
            let Some(part_address) = laid_out_mem_offsets.get(*part_id) else {
                bail!(
                    "cannot resolve address of symbol '{}' because its output section part has not been laid out yet",
                    String::from_utf8_lossy(name)
                );
            };
            let address = part_address + offset;
            let symbol_section =
                output_sections.primary_output_section(part_id.output_section_id::<P>());
            if current_section == Some(symbol_section) {
                value_kind.contains_section_relative = true;
                let section_base = section_layouts.get(symbol_section).mem_offset;
                let offset = address.checked_sub(section_base).with_context(|| {
                    format!(
                        "address of symbol '{}' is before its output section",
                        String::from_utf8_lossy(name)
                    )
                })?;
                Ok(offset)
            } else {
                value_kind.contains_absolute = true;
                Ok(address)
            }
        }
    }
}

#[derive(Default)]
struct ExpressionValueKind {
    contains_absolute: bool,
    contains_section_relative: bool,
}

impl ExpressionValueKind {
    fn needs_section_base(&self) -> bool {
        self.contains_section_relative || !self.contains_absolute
    }
}

/// True when `.` is a section offset. Top-level `. = ALIGN(...)` is an
/// absolute VMA even when the location is tagged with the previous output
/// section so that `_end = .` lands in `.brk` rather than `SHN_ABS`.
fn location_is_section_relative(
    expr_loc: &SymbolLoc,
    resolved_location_counters: &[ResolvedLocationCounter],
) -> bool {
    match expr_loc {
        SymbolLoc::SectionStartRelative(_) | SymbolLoc::SectionEndRelative(_) => true,
        SymbolLoc::LocationCounter(idx, Some(_)) => resolved_location_counters
            .get(*idx)
            .is_some_and(|entry| entry.section_offset.is_some()),
        _ => false,
    }
}

fn evaluate_location<'data, P: Platform>(
    expr_loc: &SymbolLoc,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
    resolved_location_counters: &[ResolvedLocationCounter],
) -> Result<u64> {
    match expr_loc {
        SymbolLoc::SectionStartRelative(_) => Ok(0),
        SymbolLoc::SectionEndRelative(id) => {
            let primary_id = output_sections.primary_output_section(*id);
            let primary_start = section_layouts.get(primary_id).mem_offset;
            let id_layout = section_layouts.get(*id);
            let id_end = id_layout.mem_offset + id_layout.mem_size;
            Ok(id_end - primary_start)
        }
        SymbolLoc::SectionEnd(id) => Ok(section_mem_end(*id, section_layouts, output_sections)),
        SymbolLoc::FirstSection | SymbolLoc::None => Ok(0),
        SymbolLoc::LocationCounter(idx, _) => {
            let entry = resolved_location_counters.get(*idx).ok_or_else(|| {
                crate::error!(
                    "location counter index {idx} out of range (len: {})",
                    resolved_location_counters.len()
                )
            })?;
            if location_is_section_relative(expr_loc, resolved_location_counters) {
                Ok(entry.section_offset.unwrap_or(0))
            } else {
                Ok(entry.value)
            }
        }
    }
}

/// Absolute VMA of the location counter. Inside a section, `evaluate_location` is relative to
/// the section start; GNU ld's one-arg `ALIGN(n)` aligns the absolute address.
fn absolute_location_counter<'data, P: Platform>(
    expr_loc: &SymbolLoc,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
    resolved_location_counters: &[ResolvedLocationCounter],
) -> Result<u64> {
    let relative = evaluate_location(
        expr_loc,
        section_layouts,
        output_sections,
        resolved_location_counters,
    )?;
    let base = if location_is_section_relative(expr_loc, resolved_location_counters) {
        match expr_loc {
            SymbolLoc::SectionStartRelative(id)
            | SymbolLoc::SectionEndRelative(id)
            | SymbolLoc::LocationCounter(_, Some(id)) => {
                let primary_id = output_sections.primary_output_section(*id);
                section_layouts.get(primary_id).mem_offset
            }
            _ => 0,
        }
    } else {
        0
    };
    Ok(relative.wrapping_add(base))
}

pub(crate) fn evaluate_expression<'data, P: Platform>(
    expr: &Expression<'data>,
    expr_loc: &SymbolLoc,
    input_ref: Option<&InputRef<'data>>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
    memory_regions: &HashMap<&[u8], layout::MemoryRegion>,
    symbol_db: &SymbolDb<'data, P>,
    sizeof_headers: u64,
    resolved_location_counters: &[ResolvedLocationCounter],
    laid_out_mem_offsets: &OutputSectionPartMap<Option<u64>>,
    symbol_resolution_callback: &mut dyn FnMut(&[u8]) -> Result<SymbolValue>,
) -> Result<u64> {
    let mut value_kind = ExpressionValueKind::default();
    let value = evaluate_expression_value(
        expr,
        expr_loc,
        input_ref,
        section_layouts,
        output_sections,
        memory_regions,
        symbol_db,
        sizeof_headers,
        resolved_location_counters,
        &mut value_kind,
        laid_out_mem_offsets,
        symbol_resolution_callback,
    )?;

    let offset = if value_kind.needs_section_base()
        && location_is_section_relative(expr_loc, resolved_location_counters)
    {
        if let Some(id) = expr_loc.relative_section_id() {
            let primary_id = output_sections.primary_output_section(id);
            let section_layout = section_layouts.get(primary_id);
            section_layout.mem_offset
        } else {
            0
        }
    } else {
        0
    };

    Ok(value + offset)
}

fn evaluate_expression_value<'data, P: Platform>(
    expr: &Expression<'data>,
    expr_loc: &SymbolLoc,
    input_ref: Option<&InputRef<'data>>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
    memory_regions: &HashMap<&[u8], layout::MemoryRegion>,
    symbol_db: &SymbolDb<'data, P>,
    sizeof_headers: u64,
    resolved_location_counters: &[ResolvedLocationCounter],
    value_kind: &mut ExpressionValueKind,
    laid_out_mem_offsets: &OutputSectionPartMap<Option<u64>>,
    symbol_resolution_callback: &mut dyn FnMut(&[u8]) -> Result<SymbolValue>,
) -> Result<u64> {
    macro_rules! eval_with {
        ($e:expr, $kind:expr) => {
            evaluate_expression_value(
                $e,
                expr_loc,
                input_ref,
                section_layouts,
                output_sections,
                memory_regions,
                symbol_db,
                sizeof_headers,
                resolved_location_counters,
                $kind,
                laid_out_mem_offsets,
                symbol_resolution_callback,
            )
        };
    }

    macro_rules! eval {
        ($e:expr) => {
            eval_with!($e, value_kind)
        };
    }

    macro_rules! eval_abs {
        ($e:expr) => {
            evaluate_expression(
                $e,
                expr_loc,
                input_ref,
                section_layouts,
                output_sections,
                memory_regions,
                symbol_db,
                sizeof_headers,
                resolved_location_counters,
                laid_out_mem_offsets,
                symbol_resolution_callback,
            )
        };
    }

    match expr {
        Expression::Number(n) => Ok(*n),

        Expression::LocationCounter => {
            if expr_loc.relative_section_id().is_some() {
                value_kind.contains_section_relative = true;
            } else {
                value_kind.contains_absolute = true;
            }
            evaluate_location(
                expr_loc,
                section_layouts,
                output_sections,
                resolved_location_counters,
            )
        }

        Expression::Symbol(name) => {
            let value = symbol_resolution_callback(name)?;
            evaluate_symbol_value(
                &value,
                expr_loc,
                output_sections,
                section_layouts,
                laid_out_mem_offsets,
                value_kind,
                name,
            )
        }

        Expression::Absolute(inner) => {
            value_kind.contains_absolute = true;
            eval_abs!(inner)
        }

        Expression::Add(l, r) => Ok(eval!(l)?.wrapping_add(eval!(r)?)),
        Expression::Subtract(l, r) => {
            // GNU ld: two relocatable terms cancel, so `_etext - _stext` is absolute.
            let mut left_kind = ExpressionValueKind::default();
            let mut right_kind = ExpressionValueKind::default();
            let left = eval_with!(l, &mut left_kind)?;
            let right = eval_with!(r, &mut right_kind)?;
            if left_kind.contains_section_relative && right_kind.contains_section_relative {
                value_kind.contains_absolute = true;
            } else {
                value_kind.contains_section_relative |=
                    left_kind.contains_section_relative || right_kind.contains_section_relative;
                value_kind.contains_absolute |=
                    left_kind.contains_absolute || right_kind.contains_absolute;
            }
            Ok(left.wrapping_sub(right))
        }
        Expression::Multiply(l, r) => Ok(eval!(l)?.wrapping_mul(eval!(r)?)),
        Expression::Divide(l, r) => {
            let divisor = eval!(r)?;
            if divisor == 0 {
                bail!("Division by zero in linker script expression");
            }
            Ok(((eval!(l)? as i64).wrapping_div(divisor as i64)) as u64)
        }
        Expression::Modulo(l, r) => {
            let divisor = eval!(r)?;
            if divisor == 0 {
                bail!("Modulo by zero in linker script expression");
            }
            Ok(((eval!(l)? as i64).wrapping_rem(divisor as i64)) as u64)
        }

        // Comparisons return 1 (true) or 0 (false)
        Expression::LessThan(l, r) => Ok(u64::from(eval_abs!(l)? < eval_abs!(r)?)),
        Expression::GreaterThan(l, r) => Ok(u64::from(eval_abs!(l)? > eval_abs!(r)?)),
        Expression::LessEqual(l, r) => Ok(u64::from(eval_abs!(l)? <= eval_abs!(r)?)),
        Expression::GreaterEqual(l, r) => Ok(u64::from(eval_abs!(l)? >= eval_abs!(r)?)),
        Expression::Equal(l, r) => Ok(u64::from(eval_abs!(l)? == eval_abs!(r)?)),
        Expression::NotEqual(l, r) => Ok(u64::from(eval_abs!(l)? != eval_abs!(r)?)),

        Expression::Sizeof(name) => Ok(section_size(name, section_layouts, output_sections)),
        Expression::Alignof(name) => Ok(section_align(name, section_layouts, output_sections)),
        Expression::Addr(name) => {
            value_kind.contains_absolute = true;
            section_address(name, section_layouts, output_sections)
        }

        Expression::Loadaddr(name) => {
            value_kind.contains_absolute = true;
            section_load_address(name, section_layouts, output_sections)
        }

        Expression::Align(exponent, expr) => {
            let align = eval!(exponent)?;
            if align == 0 {
                bail!("ALIGN(0) is invalid");
            }
            if let Some(e) = expr.as_ref() {
                // Two-arg ALIGN(value, align) - used by ASSERT(ALIGN(0x38, 16) == 0x40).
                Ok(eval!(e)?.next_multiple_of(align))
            } else {
                // One-arg ALIGN(n) aligns the location counter's absolute VMA, matching GNU ld.
                value_kind.contains_absolute = true;
                Ok(absolute_location_counter(
                    expr_loc,
                    section_layouts,
                    output_sections,
                    resolved_location_counters,
                )?
                .next_multiple_of(align))
            }
        }

        Expression::Min(l, r) => Ok(eval!(l)?.min(eval!(r)?)),
        Expression::Max(l, r) => Ok(eval!(l)?.max(eval!(r)?)),
        Expression::BitwiseAnd(l, r) => Ok(eval!(l)? & eval!(r)?),
        Expression::BitwiseOr(l, r) => Ok(eval!(l)? | eval!(r)?),
        Expression::BitwiseXor(l, r) => Ok(eval!(l)? ^ eval!(r)?),
        Expression::LeftShift(l, r) => Ok(eval!(l)?.wrapping_shl(eval!(r)? as u32)),
        Expression::RightShift(l, r) => Ok(eval!(l)?.wrapping_shr(eval!(r)? as u32)),
        Expression::LogicalAnd(l, r) => Ok(u64::from(eval!(l)? != 0 && eval!(r)? != 0)),
        Expression::LogicalOr(l, r) => Ok(u64::from(eval!(l)? != 0 || eval!(r)? != 0)),
        Expression::LogicalNot(e) => Ok(u64::from(eval!(e)? == 0)),
        Expression::BitwiseNot(e) => Ok(!eval!(e)?),
        Expression::Negate(e) => Ok(eval!(e)?.wrapping_neg()),

        Expression::Origin(name) => {
            value_kind.contains_absolute = true;
            let region = memory_regions.get(name).ok_or_else(|| {
                crate::error!(
                    "ORIGIN: memory region '{}' not found",
                    String::from_utf8_lossy(name)
                )
            })?;
            Ok(region.origin)
        }
        Expression::Length(name) => {
            let region = memory_regions.get(name).ok_or_else(|| {
                crate::error!(
                    "LENGTH: memory region '{}' not found",
                    String::from_utf8_lossy(name)
                )
            })?;
            Ok(region.length)
        }
        Expression::SegmentStart(name, default_expr) => {
            value_kind.contains_absolute = true;
            if let Some(val) = symbol_db.args.segment_start_override(*name) {
                Ok(val)
            } else {
                eval!(default_expr)
            }
        }
        Expression::SizeofHeaders => Ok(sizeof_headers),
        Expression::Ternary(cond, if_true, if_false) => {
            let cond = eval!(cond)?;
            if cond != 0 {
                eval!(if_true)
            } else {
                eval!(if_false)
            }
        }
        Expression::Defined(name) => Ok(symbol_db
            .get_unversioned(&UnversionedSymbolName::prehashed(name))
            .map_or(0, |_| 1)),
        Expression::Assert(assert_command) => {
            let result = eval!(&assert_command.expression)?;
            if result == 0 {
                let msg = String::from_utf8_lossy(assert_command.message);
                let Some(input_ref) = input_ref else {
                    bail!("{msg}");
                };
                let line = line_number(input_ref.data(), assert_command.remainder);
                bail!("{}:{}: {msg}", input_ref, line);
            }
            Ok(result)
        }
    }
}
fn section_size<'data, P: Platform>(
    name: &[u8],
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
) -> u64 {
    // GNU ld returns 0 for SIZEOF of a section that doesn't exist in the output.
    // We match that behavior to avoid breaking scripts that guard with SIZEOF.
    let Some(id) = output_sections.section_id_by_name(SectionName(name)) else {
        return 0;
    };
    section_mem_end(id, section_layouts, output_sections)
        .saturating_sub(section_layouts.get(id).mem_offset)
}

fn section_align<'data, P: Platform>(
    name: &[u8],
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
) -> u64 {
    // GNU ld returns 0 for ALIGNOF of a section that doesn't exist in the output.
    // We match that behavior to avoid breaking scripts that guard with SIZEOF.
    let Some(id) = output_sections.section_id_by_name(SectionName(name)) else {
        return 0;
    };
    section_layouts.get(id).alignment.value()
}

fn section_address<'data, P: Platform>(
    name: &[u8],
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
) -> Result<u64> {
    let id = output_sections
        .section_id_by_name(SectionName(name))
        .ok_or_else(|| {
            crate::error!(
                "ADDR: section '{}' not found",
                String::from_utf8_lossy(name)
            )
        })?;
    Ok(section_layouts.get(id).mem_offset)
}

fn section_load_address<'data, P: Platform>(
    name: &[u8],
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
) -> Result<u64> {
    let id = output_sections
        .section_id_by_name(SectionName(name))
        .ok_or_else(|| {
            crate::error!(
                "LOADADDR: section '{}' not found",
                String::from_utf8_lossy(name)
            )
        })?;
    Ok(section_layouts.get(id).lma_offset)
}
