use super::*;
use crate::bail;
use crate::error::Result;
use crate::grouping::Group;
use crate::layout;
use crate::layout::FileLayoutState;
use crate::layout::GroupState;
use crate::layout::InputSectionPositions;
use crate::layout::MemoryRegion;
use crate::layout::OutputRecordLayout;
use crate::layout_rules::SectionKind;
use crate::linker_script::Expression;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::SymbolLoc;
use crate::parsing::SymbolPlacement;
use crate::platform::Platform;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use hashbrown::HashMap;
use hashbrown::HashSet;
use std::cell::OnceCell;

pub(crate) fn evaluate_const<'data>(expr: &Expression<'data>) -> Result<u64> {
    evaluate_const_with_symbols(expr, &HashMap::new())
}

/// Like [`evaluate_const`], but `Expression::Symbol` may resolve through `known`.
/// Used to fold chains of constant script assignments (`later = BASE + OFFSET`)
/// before layout, matching GNU ld's forward constant references.
pub(crate) fn evaluate_const_with_symbols<'data>(
    expr: &Expression<'data>,
    known: &HashMap<&[u8], u64>,
) -> Result<u64> {
    let eval = |e: &Expression<'data>| evaluate_const_with_symbols(e, known);
    match expr {
        Expression::Number(n) => Ok(*n),
        Expression::Symbol(name) => {
            let Some(value) = known.get(name) else {
                bail!("Expected constant expression");
            };
            Ok(*value)
        }
        Expression::Add(l, r) => Ok(eval(l)?.wrapping_add(eval(r)?)),
        Expression::Subtract(l, r) => Ok(eval(l)?.wrapping_sub(eval(r)?)),
        Expression::Multiply(l, r) => Ok(eval(l)?.wrapping_mul(eval(r)?)),
        Expression::Divide(l, r) => {
            let divisor = eval(r)?;
            if divisor == 0 {
                bail!("Division by zero in linker script expression");
            }
            Ok(((eval(l)? as i64).wrapping_div(divisor as i64)) as u64)
        }
        Expression::Modulo(l, r) => {
            let divisor = eval(r)?;
            if divisor == 0 {
                bail!("Modulo by zero in linker script expression");
            }
            Ok(((eval(l)? as i64).wrapping_rem(divisor as i64)) as u64)
        }
        Expression::LessThan(l, r) => Ok(u64::from(eval(l)? < eval(r)?)),
        Expression::GreaterThan(l, r) => Ok(u64::from(eval(l)? > eval(r)?)),
        Expression::LessEqual(l, r) => Ok(u64::from(eval(l)? <= eval(r)?)),
        Expression::GreaterEqual(l, r) => Ok(u64::from(eval(l)? >= eval(r)?)),
        Expression::Equal(l, r) => Ok(u64::from(eval(l)? == eval(r)?)),
        Expression::NotEqual(l, r) => Ok(u64::from(eval(l)? != eval(r)?)),
        Expression::Min(l, r) => Ok(eval(l)?.min(eval(r)?)),
        Expression::Max(l, r) => Ok(eval(l)?.max(eval(r)?)),
        Expression::BitwiseAnd(l, r) => Ok(eval(l)? & eval(r)?),
        Expression::BitwiseOr(l, r) => Ok(eval(l)? | eval(r)?),
        Expression::BitwiseXor(l, r) => Ok(eval(l)? ^ eval(r)?),
        Expression::LeftShift(l, r) => Ok(eval(l)?.wrapping_shl(eval(r)? as u32)),
        Expression::RightShift(l, r) => Ok(eval(l)?.wrapping_shr(eval(r)? as u32)),
        Expression::LogicalAnd(l, r) => Ok(u64::from(eval(l)? != 0 && eval(r)? != 0)),
        Expression::LogicalOr(l, r) => Ok(u64::from(eval(l)? != 0 || eval(r)? != 0)),
        Expression::LogicalNot(expression) => Ok(u64::from(eval(expression)? == 0)),
        Expression::BitwiseNot(expression) => Ok(!eval(expression)?),
        Expression::Negate(expression) => Ok(eval(expression)?.wrapping_neg()),
        Expression::Ternary(cond, if_true, if_false) => {
            let cond = eval(cond)?;
            if cond != 0 {
                eval(if_true)
            } else {
                eval(if_false)
            }
        }
        Expression::Absolute(expression) => eval(expression),

        _ => bail!("Expected constant expression"),
    }
}

/// End VMA of `section_id`, including secondary contributions that have not yet been
/// merged into the primary. Needed for `_etext = .` / `text_size = _etext - _stext` while
/// later sections (`.orc_lookup`) are still being laid out.
pub(crate) fn section_mem_end<'data, P: Platform>(
    section_id: OutputSectionId,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
) -> u64 {
    let primary = output_sections.primary_output_section(section_id);
    let primary_layout = section_layouts.get(primary);
    let mut end = primary_layout.mem_offset + primary_layout.mem_size;
    for (id, info) in output_sections.ids_with_info() {
        if matches!(info.kind, SectionKind::Secondary(p) if p == primary) {
            let layout = section_layouts.get(id);
            end = end.max(layout.mem_offset + layout.mem_size);
        }
    }
    end
}

pub(crate) fn evaluate_early_expression<'data, P: Platform>(
    expr: &Expression<'data>,
    loc: &SymbolLoc,
    memory_regions: &HashMap<&[u8], MemoryRegion>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    resolved_lc: &[ResolvedLocationCounter],
    laid_out_mem_offsets: &OutputSectionPartMap<Option<u64>>,
    group_states: &[GroupState<'data, P>],
    sizes: &OutputSectionPartMap<u64>,
    output_sections: &OutputSections<'data, P>,
    symbol_db: &SymbolDb<'data, P>,
    sizeof_headers: u64,
    section_positions: &OnceCell<InputSectionPositions>,
    visited_nodes: &mut HashSet<SymbolId>,
    const_script_symbols: &HashMap<&[u8], u64>,
) -> Result<u64> {
    crate::expression_eval::evaluate_expression(
        expr,
        loc,
        None,
        section_layouts,
        output_sections,
        memory_regions,
        symbol_db,
        sizeof_headers,
        resolved_lc,
        laid_out_mem_offsets,
        &mut |name| {
            if let Some(&value) = const_script_symbols.get(name) {
                return Ok(SymbolValue::Absolute(value));
            }

            let Some(symbol_id) =
                symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(name))
            else {
                bail!(
                    "undefined symbol '{}' in linker script expression",
                    String::from_utf8_lossy(name)
                );
            };

            let canonical_id = symbol_db.definition(symbol_id);
            let file_id = symbol_db.file_id_for_symbol(canonical_id);
            let file = group_states
                .get(file_id.group())
                .and_then(|group| group.files.get(file_id.file()));
            match file {
                Some(FileLayoutState::Object(obj)) => layout::resolve_early_object_symbol(
                    canonical_id,
                    obj,
                    section_positions.get_or_init(|| {
                        layout::compute_input_section_positions(
                            group_states,
                            sizes.new_empty_like(),
                            symbol_db,
                            output_sections,
                        )
                    }),
                    symbol_db,
                ),
                Some(FileLayoutState::LinkerScript(ls))
                    if let Group::LinkerScripts(scripts) = &symbol_db.groups[file_id.group()] =>
                {
                    let script = &scripts[file_id.file()];
                    let symbol_offset = ls.symbol_id_range.id_to_offset(canonical_id);

                    let def_info = &script.parsed.symbol_defs[symbol_offset];
                    evaluate_early_expression_internal_symbol(
                        memory_regions,
                        section_layouts,
                        resolved_lc,
                        laid_out_mem_offsets,
                        group_states,
                        sizes,
                        output_sections,
                        symbol_db,
                        sizeof_headers,
                        section_positions,
                        visited_nodes,
                        const_script_symbols,
                        canonical_id,
                        def_info,
                    )
                }
                _ => Ok(SymbolValue::Absolute(layout::layout_time_symbol_value(
                    name,
                    symbol_db,
                    section_layouts,
                    output_sections,
                    memory_regions,
                    loc,
                    sizeof_headers,
                    resolved_lc,
                    const_script_symbols,
                    0,
                )?)),
            }
        },
    )
}

fn evaluate_early_expression_internal_symbol<'data, P: Platform>(
    memory_regions: &HashMap<&[u8], MemoryRegion>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    resolved_lc: &[ResolvedLocationCounter],
    laid_out_mem_offsets: &OutputSectionPartMap<Option<u64>>,
    group_states: &[GroupState<'data, P>],
    sizes: &OutputSectionPartMap<u64>,
    output_sections: &OutputSections<'data, P>,
    symbol_db: &SymbolDb<'data, P>,
    sizeof_headers: u64,
    section_positions: &OnceCell<InputSectionPositions>,
    visited_nodes: &mut HashSet<SymbolId>,
    const_script_symbols: &HashMap<&[u8], u64>,
    canonical_id: SymbolId,
    def_info: &crate::parsing::InternalSymDefInfo<'data, P>,
) -> Result<SymbolValue> {
    match &def_info.placement {
        SymbolPlacement::Redirect(redirect) => {
            if !visited_nodes.insert(canonical_id) {
                return Ok(SymbolValue::Absolute(0));
            }
            let value = evaluate_early_expression(
                &redirect.expression,
                &redirect.loc,
                memory_regions,
                section_layouts,
                resolved_lc,
                laid_out_mem_offsets,
                group_states,
                sizes,
                output_sections,
                symbol_db,
                sizeof_headers,
                section_positions,
                visited_nodes,
                const_script_symbols,
            );
            visited_nodes.remove(&canonical_id);
            let value = value?;
            let symbol_section = redirect
                .loc
                .relative_section_id()
                .map(|id| output_sections.primary_output_section(id));
            if let Some(symbol_section) = symbol_section {
                Ok(SymbolValue::SectionRelative {
                    section_id: symbol_section,
                    address: value,
                })
            } else {
                Ok(SymbolValue::Absolute(value))
            }
        }
        SymbolPlacement::SectionStart(section_id) => Ok(SymbolValue::SectionRelative {
            section_id: *section_id,
            address: 0,
        }),
        SymbolPlacement::SectionEnd(section_id) => Ok(SymbolValue::SectionRelative {
            section_id: *section_id,
            address: section_layouts.get(*section_id).mem_size,
        }),
        _ => {
            bail!("Unsupported symbol type");
        }
    }
}
