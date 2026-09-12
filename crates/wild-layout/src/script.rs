use super::types::{
    FileLayoutState, GroupState, InputSortedSection, MemoryRegion, OutputRecordLayout,
    object_symbol_address_in_layout,
};
use crate::expression_eval::{
    ResolvedLocationCounter, SymbolValue, evaluate_const, evaluate_const_with_symbols,
};
use crate::grouping::{Group, SequencedInput};
use crate::output_section_id::{OutputSectionId, OutputSections, SectionName};
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::{InternalSymDefInfo, SymbolLoc, SymbolPlacement};
use crate::part_id::PartId;
use crate::resolution::SectionSlot;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::{EnginePlatform, timing_phase};
use hashbrown::{HashMap, HashSet};
use object::SectionIndex;
use wild_error::bail;
use wild_error::error::Result;
use wild_platform::output_section_map::OutputSectionMap;
use wild_platform::{ObjectFile, RelocationList as _};
use wild_scripts::linker_script::{Expression, NocrossrefConstraint};

/// BYTE/SHORT/LONG/QUAD advance the location counter via a trailing secondary section that has no
/// input parts. Grow the primary section so the writer buffer covers those bytes.
pub fn extend_sections_for_script_output_data<P: EnginePlatform>(
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
pub fn script_phdrs_writable<P: EnginePlatform>(
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
pub fn layout_time_symbol_value<'data, P: EnginePlatform>(
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

pub fn script_def_layout_value<'data, P: EnginePlatform>(
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
pub fn script_assignment_def<'data, 's, P: EnginePlatform>(
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

pub fn collect_const_script_symbols<'data, P: EnginePlatform>(
    symbol_db: &SymbolDb<'data, P>,
) -> HashMap<&'data [u8], u64> {
    let mut candidates = Vec::new();
    for group in &symbol_db.groups {
        match group {
            Group::Prelude(prelude) => {
                collect_const_candidates(&prelude.symbol_definitions, &mut candidates);
            }
            Group::LinkerScripts(scripts) => {
                for script in scripts {
                    collect_const_candidates(&script.parsed.symbol_defs, &mut candidates);
                }
            }
            _ => {}
        }
    }
    resolve_const_candidates(&candidates)
}

fn collect_const_candidates<'a, 'data, P: EnginePlatform>(
    defs: &'a [InternalSymDefInfo<'data, P>],
    candidates: &mut Vec<(&'data [u8], &'a Expression<'data>)>,
) {
    for def in defs {
        if def.name.is_empty() {
            continue;
        }
        let SymbolPlacement::Redirect(redirect) = &def.placement else {
            continue;
        };
        candidates.push((def.name, &redirect.expression));
    }
}

/// Fold constant assignments, including chains and later definitions
/// (`later_sum = BASE + OFFSET` after `. = later_sum`). Later assignments of
/// the same name win. Location-dependent RHSs (`.`, `ADDR`, …) stay unresolved.
fn resolve_const_candidates<'data>(
    candidates: &[(&'data [u8], &Expression<'data>)],
) -> HashMap<&'data [u8], u64> {
    let mut map = HashMap::new();
    let mut owner: HashMap<&[u8], usize> = HashMap::new();
    // One new name per pass in the worst case (a chain of forward refs).
    for _ in 0..=candidates.len() {
        let mut progress = false;
        for (i, &(name, expr)) in candidates.iter().enumerate() {
            if owner.get(name).is_some_and(|&j| j >= i) {
                continue;
            }
            let Ok(value) = evaluate_const_with_symbols(expr, &map) else {
                continue;
            };
            map.insert(name, value);
            owner.insert(name, i);
            progress = true;
        }
        if !progress {
            break;
        }
    }
    map
}

pub fn harvest_and_sort_script_sections<'data, P: EnginePlatform>(
    group_states: &mut [GroupState<'data, P>],
    output_sections: &mut OutputSections<P>,
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

    struct Harvested<'data> {
        sort_by_init_priority: bool,
        sort_by_alignment: bool,
        sort_by_name: bool,
        sort_name_primary: bool,
        sort_reversed: bool,
        name: &'data [u8],
        section: InputSortedSection,
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
                        sections_out.push(Harvested {
                            sort_by_init_priority: sorted_section.sort_by_init_priority,
                            sort_by_alignment: sorted_section.sort_by_alignment,
                            sort_by_name: sorted_section.sort_by_name,
                            sort_name_primary: sorted_section.sort_name_primary,
                            sort_reversed: sorted_section.sort_reversed,
                            name,
                            section: InputSortedSection {
                                file_id: obj.file_id,
                                section_index: sorted_section.index,
                                part_id,
                                size: capacity,
                                alignment: sec.section.alignment,
                            },
                        });
                    }
                }
            }
        }
    }

    for harvested in &sections_out {
        if harvested.sort_by_name {
            output_sections.bump_min_alignment(
                harvested.section.part_id.output_section_id::<P>(),
                harvested.section.alignment,
            );
        }
    }

    sections_out.sort_by(|a, b| {
        a.section.part_id.cmp(&b.section.part_id).then_with(|| {
            match (a.sort_by_init_priority, b.sort_by_init_priority) {
                (true, true) => {
                    let pa = P::init_section_priority(a.name).unwrap_or(u16::MAX);
                    let pb = P::init_section_priority(b.name).unwrap_or(u16::MAX);
                    let ord = pa.cmp(&pb).then_with(|| a.name.cmp(b.name));
                    if a.sort_reversed && b.sort_reversed {
                        ord.reverse()
                    } else {
                        ord
                    }
                }
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) if a.sort_by_alignment && b.sort_by_alignment => {
                    if a.sort_by_name && b.sort_by_name {
                        match (a.sort_name_primary, b.sort_name_primary) {
                            (true, true) => a
                                .name
                                .cmp(b.name)
                                .then_with(|| b.section.alignment.cmp(&a.section.alignment)),
                            (false, false) => b
                                .section
                                .alignment
                                .cmp(&a.section.alignment)
                                .then_with(|| a.name.cmp(b.name)),
                            (true, false) => std::cmp::Ordering::Less,
                            (false, true) => std::cmp::Ordering::Greater,
                        }
                    } else {
                        // GNU `SORT_BY_ALIGNMENT` only: largest alignment first, then
                        // input order (stable). Reverse alignment is not supported.
                        b.section.alignment.cmp(&a.section.alignment).then_with(|| {
                            a.section.file_id.cmp(&b.section.file_id).then_with(|| {
                                a.section.section_index.0.cmp(&b.section.section_index.0)
                            })
                        })
                    }
                }
                (false, false) if a.sort_by_alignment => std::cmp::Ordering::Less,
                (false, false) if b.sort_by_alignment => std::cmp::Ordering::Greater,
                (false, false) => {
                    let ord = a.name.cmp(b.name);
                    if a.sort_reversed && b.sort_reversed {
                        ord.reverse()
                    } else {
                        ord
                    }
                }
            }
        })
    });
    sections_out
        .into_iter()
        .map(|harvested| harvested.section)
        .collect()
}

struct ResolvedNocrossref {
    to: Option<OutputSectionId>,
    sections: HashSet<OutputSectionId>,
}

pub fn check_nocrossrefs<'data, P: EnginePlatform>(
    group_states: &[GroupState<'data, P>],
    symbol_db: &SymbolDb<'data, P>,
    output_sections: &OutputSections<'data, P>,
) -> Result {
    let mut constraints = Vec::new();
    for group in &symbol_db.groups {
        let Group::LinkerScripts(scripts) = group else {
            continue;
        };
        for script in scripts {
            for constraint in &script.parsed.nocrossrefs {
                if let Some(resolved) = resolve_nocrossref(constraint, output_sections) {
                    constraints.push(resolved);
                }
            }
        }
    }
    if constraints.is_empty() {
        return Ok(());
    }

    for group in group_states {
        for file in &group.files {
            let FileLayoutState::Object(obj) = file else {
                continue;
            };
            for (sec_idx, slot) in obj.sections.iter().enumerate() {
                if !section_slot_has_relocs(slot) {
                    continue;
                }
                let from_id = output_sections.primary_output_section(
                    obj.section_part_id(SectionIndex(sec_idx), &symbol_db.section_part_ids)
                        .output_section_id::<P>(),
                );
                let Ok(relocs) = obj
                    .object
                    .relocations(SectionIndex(sec_idx), &obj.relocations)
                else {
                    continue;
                };
                let mut error = None;
                relocs.for_each_symbol(&mut |sym_idx| {
                    if error.is_some() {
                        return;
                    }
                    let local_id = obj.symbol_id_range.input_to_id(sym_idx);
                    let def_id = symbol_db.definition(local_id);
                    let Some(to_id) = symbol_db.output_section_id(def_id) else {
                        return;
                    };
                    let to_id = output_sections.primary_output_section(to_id);
                    if from_id == to_id {
                        return;
                    }
                    if !constraints.iter().any(|c| c.forbids(from_id, to_id)) {
                        return;
                    }
                    let from_name = output_sections.display_name(from_id);
                    let to_name = output_sections.display_name(to_id);
                    let symbol_name = symbol_db.symbol_name_for_display(def_id);
                    error = Some(wild_error::error!(
                        "prohibited cross reference from {from_name} to `{symbol_name}` in {to_name}"
                    ));
                });
                if let Some(error) = error {
                    return Err(error);
                }
            }
        }
    }

    Ok(())
}

fn resolve_nocrossref<'data, P: EnginePlatform>(
    constraint: &NocrossrefConstraint<'data>,
    output_sections: &OutputSections<'data, P>,
) -> Option<ResolvedNocrossref> {
    let lookup = |name: &[u8]| output_sections.section_id_by_name(SectionName(name));
    let to = constraint.to.and_then(lookup);
    if constraint.to.is_some() && to.is_none() {
        return None;
    }
    let sections: HashSet<OutputSectionId> = constraint
        .sections
        .iter()
        .copied()
        .filter_map(lookup)
        .collect();
    if constraint.to.is_none() && sections.len() < 2 {
        return None;
    }
    if constraint.to.is_some() && sections.is_empty() {
        return None;
    }
    Some(ResolvedNocrossref { to, sections })
}

impl ResolvedNocrossref {
    fn forbids(&self, from: OutputSectionId, to: OutputSectionId) -> bool {
        if let Some(forbidden_to) = self.to {
            return to == forbidden_to && self.sections.contains(&from);
        }
        self.sections.contains(&from) && self.sections.contains(&to)
    }
}

fn section_slot_has_relocs(slot: &SectionSlot) -> bool {
    matches!(
        slot,
        SectionSlot::Loaded(_)
            | SectionSlot::Sorted(_)
            | SectionSlot::LoadedDebugInfo(_)
            | SectionSlot::MergeStrings(_)
            | SectionSlot::PartialLinkSingleton(_)
    )
}
