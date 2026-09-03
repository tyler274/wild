use super::types::*;
use crate::bail;
use crate::error::Result;
use crate::expression_eval::ResolvedLocationCounter;
use crate::expression_eval::SymbolValue;
use crate::expression_eval::evaluate_const;
use crate::grouping::Group;
use crate::grouping::SequencedInput;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::SymbolLoc;
use crate::parsing::SymbolPlacement;
use crate::part_id::PartId;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::resolution::SectionSlot;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use hashbrown::HashMap;

/// BYTE/SHORT/LONG/QUAD advance the location counter via a trailing secondary section that has no
/// input parts. Grow the primary section so the writer buffer covers those bytes.
pub(crate) fn extend_sections_for_script_output_data<P: Platform>(
    output_sections: &OutputSections<P>,
    section_layouts: &mut OutputSectionMap<OutputRecordLayout>,
    resolved_location_counters: &[ResolvedLocationCounter],
) {
    for data in &output_sections.script_output_data {
        let Some(lc) = resolved_location_counters.get(data.location_counter_index) else {
            continue;
        };
        let needed = lc.section_offset.unwrap_or(u64::from(data.width)) as usize;
        let layout = section_layouts.get_mut(data.section_id);
        if needed > layout.file_size && output_sections.has_data_in_file(data.section_id) {
            layout.file_size = needed;
        }
        if needed as u64 > layout.mem_size {
            layout.mem_size = needed as u64;
        }
    }
}

/// GNU ld copies `PF_W` from an assigned `PT_LOAD` onto script-only output
/// sections that have no input flags to inherit (kernel `.orc_lookup`).
pub(crate) fn script_phdrs_writable<P: Platform>(
    phdr_names: &[&[u8]],
    symbol_db: &SymbolDb<P>,
) -> bool {
    for group in &symbol_db.groups {
        let Group::LinkerScripts(scripts) = group else {
            continue;
        };
        for script in scripts {
            for phdr in &script.parsed.program_headers {
                if !phdr_names.contains(&phdr.name) {
                    continue;
                }
                let Some(flags_expr) = &phdr.flags else {
                    continue;
                };
                let Ok(flags) = evaluate_const(flags_expr) else {
                    continue;
                };
                if P::phdr_flags_writable(flags) {
                    return true;
                }
            }
        }
    }
    false
}

/// Resolve a named symbol while assigning output-section addresses.
///
/// Constant script symbols (for example `LOAD_OFFSET = 0x1000`) are used as-is. Object symbols
/// whose output section has already been laid out are `output_section.mem_offset + st_value`.
/// That is exact when the symbol's input section is the first (or only) contribution to that
/// output section or secondary, which is the GNU ld pattern used by the kernel
/// (`. = srso_alias_untrain_ret | …` after a dedicated matcher).
///
/// Linker-script symbols assigned to `.` (for example `__start_init_stack = .`) are evaluated
/// from their stored expression and location, so later `. = symbol + SIZE` commands work.
///
/// Named symbols are GNU ld "absolute" addresses, so `. = symbol | mask` applies the mask
/// to the VMA rather than adding it as a section offset.
pub(crate) fn layout_time_symbol_value<'data, P: Platform>(
    name: &[u8],
    symbol_db: &SymbolDb<'data, P>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
    memory_regions: &HashMap<&[u8], MemoryRegion>,
    loc: &SymbolLoc,
    sizeof_headers: u64,
    resolved_lc: &[ResolvedLocationCounter],
    const_script_symbols: &HashMap<&[u8], u64>,
    recursion_depth: u32,
) -> Result<u64> {
    if recursion_depth > 32 {
        bail!(
            "cyclic linker-script symbol `{}`",
            String::from_utf8_lossy(name)
        );
    }

    if let Some(value) = const_script_symbols.get(name) {
        return Ok(*value);
    }

    // A linker-script assignment (`_etext = .`) overrides the prelude's
    // `SectionEnd(.text)` of the same name. Without this, `. += text_size` during
    // layout sees `_etext == 0` because the builtin `.text` has not been merged yet
    // (or is a different section from the script's `.text`).
    if let Some(def) = script_assignment_def(name, symbol_db) {
        return script_def_layout_value(
            name,
            def,
            symbol_db,
            section_layouts,
            output_sections,
            memory_regions,
            loc,
            sizeof_headers,
            resolved_lc,
            const_script_symbols,
            recursion_depth,
        );
    }

    let Some(symbol_id) = symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(name)) else {
        bail!(
            "undefined symbol `{}` in linker-script expression",
            String::from_utf8_lossy(name)
        );
    };
    let definition = symbol_db.definition(symbol_id);
    if symbol_db.is_undefined(definition) {
        bail!(
            "undefined symbol `{}` in linker-script expression",
            String::from_utf8_lossy(name)
        );
    }

    match symbol_db.file(symbol_db.file_id_for_symbol(definition)) {
        SequencedInput::Object(obj) => {
            object_symbol_address_in_layout(name, obj, definition, symbol_db, section_layouts)
        }
        #[cfg(all(feature = "plugins", unix))]
        SequencedInput::LtoInput(_) => {
            bail!(
                "symbol `{}` is defined by an LTO input that has not been code-generated",
                String::from_utf8_lossy(name)
            );
        }
        SequencedInput::Prelude(prelude) => script_def_layout_value(
            name,
            prelude.symbol_def(definition),
            symbol_db,
            section_layouts,
            output_sections,
            memory_regions,
            loc,
            sizeof_headers,
            resolved_lc,
            const_script_symbols,
            recursion_depth,
        ),
        SequencedInput::LinkerScript(script) => {
            let offset = definition.to_offset(script.symbol_id_range);
            script_def_layout_value(
                name,
                &script.parsed.symbol_defs[offset],
                symbol_db,
                section_layouts,
                output_sections,
                memory_regions,
                loc,
                sizeof_headers,
                resolved_lc,
                const_script_symbols,
                recursion_depth,
            )
        }
        SequencedInput::SyntheticSymbols(_) | SequencedInput::StubLibrary(_) => {
            bail!(
                "Symbols with the set location operation are not yet supported (`{}`).",
                String::from_utf8_lossy(name)
            );
        }
    }
}

pub(crate) fn script_def_layout_value<'data, P: Platform>(
    name: &[u8],
    def: &InternalSymDefInfo<'data, P>,
    symbol_db: &SymbolDb<'data, P>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, P>,
    memory_regions: &HashMap<&[u8], MemoryRegion>,
    _outer_loc: &SymbolLoc,
    sizeof_headers: u64,
    resolved_lc: &[ResolvedLocationCounter],
    const_script_symbols: &HashMap<&[u8], u64>,
    recursion_depth: u32,
) -> Result<u64> {
    match &def.placement {
        SymbolPlacement::Redirect(redirect) => crate::expression_eval::evaluate_expression(
            &redirect.expression,
            &redirect.loc,
            None,
            section_layouts,
            output_sections,
            memory_regions,
            symbol_db,
            sizeof_headers,
            resolved_lc,
            &OutputSectionPartMap::default(),
            &mut |nested| {
                Ok(SymbolValue::Absolute(layout_time_symbol_value(
                    nested,
                    symbol_db,
                    section_layouts,
                    output_sections,
                    memory_regions,
                    &redirect.loc,
                    sizeof_headers,
                    resolved_lc,
                    const_script_symbols,
                    recursion_depth + 1,
                )?))
            },
        ),
        SymbolPlacement::SectionStart(id) => Ok(section_layouts.get(*id).mem_offset),
        SymbolPlacement::SectionEnd(id) | SymbolPlacement::SectionGroupEnd(id) => Ok(
            crate::expression_eval::section_mem_end(*id, section_layouts, output_sections),
        ),
        _ => bail!(
            "Symbols with the set location operation are not yet supported (`{}`).",
            String::from_utf8_lossy(name)
        ),
    }
}

/// Last non-PROVIDE linker-script assignment of `name`, if any.
pub(crate) fn script_assignment_def<'data, 's, P: Platform>(
    name: &[u8],
    symbol_db: &'s SymbolDb<'data, P>,
) -> Option<&'s InternalSymDefInfo<'data, P>> {
    let mut found = None;
    for group in &symbol_db.groups {
        let Group::LinkerScripts(scripts) = group else {
            continue;
        };
        for script in scripts {
            for def in &script.parsed.symbol_defs {
                if !def.is_provide && def.name == name {
                    found = Some(def);
                }
            }
        }
    }
    found
}

pub(crate) fn collect_const_script_symbols<'data, P: Platform>(
    symbol_db: &SymbolDb<'data, P>,
) -> HashMap<&'data [u8], u64> {
    let mut map = HashMap::new();
    for group in &symbol_db.groups {
        let defs = match group {
            Group::Prelude(prelude) => prelude.symbol_definitions.as_slice(),
            Group::LinkerScripts(scripts) => {
                for script in scripts {
                    collect_const_defs(&script.parsed.symbol_defs, &mut map);
                }
                continue;
            }
            _ => continue,
        };
        collect_const_defs(defs, &mut map);
    }
    map
}

pub(crate) fn collect_const_defs<'data, P: Platform>(
    defs: &[InternalSymDefInfo<'data, P>],
    map: &mut HashMap<&'data [u8], u64>,
) {
    for def in defs {
        if def.name.is_empty() {
            continue;
        }
        let SymbolPlacement::Redirect(redirect) = &def.placement else {
            continue;
        };
        if let Ok(value) = evaluate_const(&redirect.expression) {
            map.insert(def.name, value);
        }
    }
}

pub(crate) fn harvest_and_sort_script_sections<'data, P: Platform>(
    group_states: &mut [GroupState<'data, P>],
    output_sections: &OutputSections<P>,
    section_part_ids: &[PartId],
) -> Vec<InputSortedSection> {
    timing_phase!("Harvest and sort script sections");

    let has_any_sorting = group_states.iter().any(|g| {
        g.files.iter().any(|f| {
            if let FileLayoutState::Object(obj) = f {
                !obj.script_sorted_sections.is_empty()
            } else {
                false
            }
        })
    });

    if !has_any_sorting {
        return Vec::new();
    }

    let mut sections_out = Vec::new();
    for group in group_states.iter_mut() {
        for file in &mut group.files {
            if let FileLayoutState::Object(obj) = file {
                for sorted_section in &obj.script_sorted_sections {
                    if let SectionSlot::Sorted(sec) = &obj.sections[sorted_section.index.0] {
                        let part_id = obj.section_part_id(sorted_section.index, section_part_ids);
                        let capacity = sec.section.capacity(part_id, output_sections);
                        let name = obj
                            .object
                            .section_name(sorted_section.index)
                            .unwrap_or_default();
                        sections_out.push((
                            sorted_section.sort_by_init_priority,
                            name,
                            InputSortedSection {
                                file_id: obj.file_id,
                                section_index: sorted_section.index,
                                part_id,
                                size: capacity,
                                alignment: part_id.alignment(output_sections),
                            },
                        ));
                    }
                }
            }
        }
    }

    sections_out.sort_by(|a, b| match (a.0, b.0) {
        (true, true) => {
            let pa = P::init_section_priority(a.1).unwrap_or(u16::MAX);
            let pb = P::init_section_priority(b.1).unwrap_or(u16::MAX);
            pa.cmp(&pb).then_with(|| a.1.cmp(b.1))
        }
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => a.1.cmp(b.1),
    });
    sections_out
        .into_iter()
        .map(|(_, _, harvested)| harvested)
        .collect()
}
