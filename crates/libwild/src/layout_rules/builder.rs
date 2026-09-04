use super::*;
use crate::OutputSections;
use crate::alignment;
use crate::arch::Architecture;
use crate::error::Context;
use crate::error::Result;
use crate::expression_eval::evaluate_const;
use crate::input_data::InputLinkerScript;
use crate::input_data::InputRef;
use crate::linker_script;
use crate::linker_script::ContentsCommand;
use crate::linker_script::Expression;
use crate::linker_script::SectionCommand;
use crate::output_section_id::GnuBuildIdPlacement;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionLocationInfo;
use crate::output_section_id::SectionName;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::ProcessedLinkerScript;
use crate::parsing::Redirect;
use crate::parsing::RedirectKind;
use crate::parsing::SymbolLoc;
use crate::parsing::SymbolPlacement;
use crate::platform::Args as _;
use crate::platform::Platform;
use hashbrown::HashMap;
use linker_utils::elf::secnames::NOTE_GNU_BUILD_ID_SECTION_NAME;

#[derive(Default)]
pub(crate) struct LayoutRulesBuilder<'data> {
    rules: Vec<SectionRule<'data>>,
    num_location_counters: usize,
    overlay_group: u32,
}
fn matcher_uses_input_order(matcher: &linker_script::Matcher<'_>) -> bool {
    matcher
        .input_section_name_patterns
        .iter()
        .all(|p| p.sort == linker_script::SortKind::None)
}
fn loc_for_global_expr<'data>(
    expr: &crate::linker_script::Expression<'data>,
    section_id: Option<OutputSectionId>,
) -> SymbolLoc {
    let mut loc = SymbolLoc::None;
    expr.visit_expressions(&mut |e| match e {
        crate::linker_script::Expression::SegmentStart(..) => {
            if let Some(section_id) = section_id {
                loc = SymbolLoc::SectionEnd(section_id);
            } else {
                loc = SymbolLoc::FirstSection;
            }
            false
        }
        _ => true,
    });
    loc
}
impl<'data> LayoutRulesBuilder<'data> {
    /// Records information about any sections and symbols declared by the linker script.
    pub(crate) fn process_linker_script<P: Platform>(
        &mut self,
        input: &InputLinkerScript<'data>,
        output_sections: &mut OutputSections<'data, P>,
        args: &P::Args,
    ) -> Result<ProcessedLinkerScript<'data, P>> {
        let mut symbol_defs = Vec::new();
        let mut memory_regions = Vec::new();
        let mut program_headers = Vec::new();
        let mut location_counters = Vec::new();
        let mut ordered_sections = Vec::new();

        let mut current_section_id = None;
        let mut loc = SymbolLoc::FirstSection;
        let mut last_lc_idx = self.num_location_counters;

        for cmd in &input.script.commands {
            if let linker_script::Command::Provide(provide) = cmd {
                let placement = SymbolPlacement::Redirect(Redirect {
                    kind: RedirectKind::Script,
                    expression: provide.value.clone(),
                    loc: loc_for_global_expr(&provide.value, current_section_id),
                });
                symbol_defs.push(
                    crate::parsing::InternalSymDefInfo::new(placement, provide.name)
                        .with_provide()
                        .with_hidden(provide.hidden),
                );
            } else if let linker_script::Command::SymbolDefinition { name, value } = cmd {
                let placement = SymbolPlacement::Redirect(Redirect {
                    kind: RedirectKind::Script,
                    expression: value.to_owned(),
                    loc: loc_for_global_expr(value, current_section_id),
                });
                symbol_defs.push(crate::parsing::InternalSymDefInfo::new(placement, name));
            } else if let linker_script::Command::SetLocation(loc) = cmd {
                let placement = SymbolPlacement::Redirect(Redirect {
                    kind: RedirectKind::Script,
                    expression: loc.address.clone(),
                    loc: loc_for_global_expr(&loc.address, current_section_id),
                });
                symbol_defs.push(crate::parsing::InternalSymDefInfo::new(placement, b""));
            } else if let linker_script::Command::Sections(sections) = cmd {
                let mut section_start_lc_idx = last_lc_idx;
                let mut prev_phdrs = Vec::new();
                let mut only_if_by_name: HashMap<&[u8], OutputSectionId> = HashMap::new();
                for sec_cmd in &sections.commands {
                    match sec_cmd {
                        SectionCommand::Section(sec) => {
                            if sec.output_section_name == b"/DISCARD/" {
                                for contents_cmd in &sec.commands {
                                    match contents_cmd {
                                        ContentsCommand::Matcher(matcher) => {
                                            for pattern in &matcher.input_section_name_patterns {
                                                let rule = SectionRule::new(
                                                    pattern.name,
                                                    matcher.input_file_pattern,
                                                    crate::layout_rules::SectionRuleOutcome::Discard,
                                                )?
                                                .with_excludes(&matcher.exclude_file_patterns)?;
                                                record_gnu_build_id_placement(
                                                    output_sections,
                                                    &rule,
                                                    GnuBuildIdPlacement::Discard,
                                                );
                                                self.add_section_rule(rule);
                                            }
                                        }
                                        _ => crate::bail!("Illegal use of /DISCARD/ section"),
                                    }
                                }
                                continue;
                            }
                            // GNU `ONLY_IF_RO` then `ONLY_IF_RW` for the same name share one
                            // output section. The unused copy is dropped after we see whether
                            // any matching input is writable.
                            let existing_only_if_id = sec.only_if.and_then(|_| {
                                only_if_by_name.get(sec.output_section_name).copied()
                            });
                            let min_alignment =
                                sec.alignment.unwrap_or(alignment::MIN).max(alignment::MIN);

                            let location_info = SectionLocationInfo {
                                location_counters: (section_start_lc_idx, last_lc_idx),
                                location: sec.start_address_expression.clone(),
                                at_location: sec.at_address.clone(),
                                at_region: sec.at_region,
                                is_top_level: true,
                                overlay: None,
                            };

                            let fill_value = sec
                                .fill
                                .as_ref()
                                .map(|fill| -> Result<[u8; 4]> {
                                    let value = evaluate_const(&fill.value)?;
                                    if value > u64::from(u32::MAX) {
                                        crate::bail!(
                                            "Filler expression result does not fit 32-bit: 0x{:x}",
                                            value
                                        );
                                    }
                                    Ok((value as u32).to_be_bytes())
                                })
                                .transpose()?;

                            if !sec.phdrs.is_empty() {
                                prev_phdrs = sec.phdrs.clone();
                            }
                            let primary_section_id = if let Some(existing_id) = existing_only_if_id
                            {
                                existing_id
                            } else {
                                let identity = P::section_identity_from_name(SectionName(
                                    sec.output_section_name,
                                ))
                                .with_context(|| {
                                    format!(
                                        "Output section `{}` cannot be identified from the name alone",
                                        SectionName(sec.output_section_name)
                                    )
                                })?;
                                let id = output_sections.add_named_section(
                                    identity,
                                    min_alignment,
                                    sec.region,
                                    Some(&location_info),
                                    fill_value,
                                    prev_phdrs.clone(),
                                    sec.attributes.as_ref(),
                                );
                                if sec.only_if.is_some() {
                                    only_if_by_name.insert(sec.output_section_name, id);
                                }
                                id
                            };
                            if let Some(only_if) = sec.only_if {
                                output_sections.record_only_if(
                                    primary_section_id,
                                    only_if,
                                    ordered_sections.len(),
                                    location_info.clone(),
                                    prev_phdrs.clone(),
                                );
                            }
                            ordered_sections.push(primary_section_id);
                            current_section_id = Some(primary_section_id);
                            loc = SymbolLoc::SectionEnd(primary_section_id);

                            let mut last_section_id = None;
                            let mut last_symbol_loc =
                                SymbolLoc::SectionStartRelative(primary_section_id);
                            let mut inner_lc_idx = last_lc_idx;
                            let mut inner_lc_start_idx = last_lc_idx;

                            for contents_cmd in &sec.commands {
                                match contents_cmd {
                                    ContentsCommand::Matcher(matcher) => {
                                        let input_order = matcher_uses_input_order(matcher);
                                        let section_id = if last_section_id.is_none()
                                            && inner_lc_idx == inner_lc_start_idx
                                        {
                                            output_sections
                                                .set_input_order(primary_section_id, input_order);
                                            primary_section_id
                                        } else {
                                            let sec_location_info = SectionLocationInfo {
                                                location_counters: (
                                                    inner_lc_start_idx,
                                                    inner_lc_idx,
                                                ),
                                                location: None,
                                                at_location: None,
                                                at_region: None,
                                                is_top_level: false,
                                                overlay: None,
                                            };
                                            inner_lc_start_idx = inner_lc_idx;
                                            output_sections.add_secondary_section(
                                                primary_section_id,
                                                alignment::MIN,
                                                None,
                                                Some(sec_location_info),
                                                input_order,
                                            )
                                        };

                                        for pattern in &matcher.input_section_name_patterns {
                                            let output_info = SectionOutputInfo {
                                                section_id,
                                                must_keep: matcher.must_keep,
                                                sorted: pattern.sort.needs_sort(),
                                                sort_by_init_priority: pattern.sort
                                                    == linker_script::SortKind::InitPriority,
                                                sort_by_alignment: pattern.sort
                                                    == linker_script::SortKind::Alignment,
                                                input_order,
                                            };

                                            let outcome = SectionRuleOutcome::section_rule_from_id::<
                                                P,
                                            >(
                                                primary_section_id, output_info
                                            );

                                            let rule = SectionRule::new(
                                                pattern.name,
                                                matcher.input_file_pattern,
                                                outcome,
                                            )?
                                            .with_excludes(&matcher.exclude_file_patterns)?
                                            .with_only_if(sec.only_if, primary_section_id);
                                            record_gnu_build_id_placement(
                                                output_sections,
                                                &rule,
                                                GnuBuildIdPlacement::Merge(primary_section_id),
                                            );
                                            self.add_section_rule(rule);
                                        }

                                        last_section_id = Some(section_id);
                                        last_symbol_loc = SymbolLoc::SectionEndRelative(section_id);
                                    }
                                    ContentsCommand::SymbolAssignment(assignment) => {
                                        let placement = SymbolPlacement::Redirect(Redirect {
                                            kind: RedirectKind::Script,
                                            expression: assignment.expr.clone(),
                                            loc: last_symbol_loc.clone(),
                                        });
                                        symbol_defs.push(InternalSymDefInfo::new(
                                            placement,
                                            assignment.name,
                                        ));
                                    }
                                    ContentsCommand::Provide(provide) => {
                                        let placement = SymbolPlacement::Redirect(Redirect {
                                            kind: RedirectKind::Script,
                                            expression: provide.value.clone(),
                                            loc: last_symbol_loc.clone(),
                                        });
                                        symbol_defs.push(
                                            InternalSymDefInfo::new(placement, provide.name)
                                                .with_provide()
                                                .with_hidden(provide.hidden),
                                        );
                                    }
                                    ContentsCommand::SetLocation(location) => {
                                        location_counters.push(LocationCounter::Relative(
                                            location.address.clone(),
                                            last_symbol_loc,
                                            primary_section_id,
                                        ));
                                        last_symbol_loc = SymbolLoc::LocationCounter(
                                            inner_lc_idx,
                                            Some(primary_section_id),
                                        );
                                        inner_lc_idx += 1;
                                    }
                                    // The CONSTRUCTORS command is used in legacy file formats only.
                                    // On ELF it is a nop.
                                    // (https://sourceware.org/binutils/docs/ld/Output-Section-Keywords.html#index-CONSTRUCTORS)
                                    ContentsCommand::Constructors
                                    | ContentsCommand::LinkerVersion => (),
                                    ContentsCommand::Assert(assert_cmd) => {
                                        let placement = SymbolPlacement::Redirect(Redirect {
                                            kind: RedirectKind::Script,
                                            expression: Expression::Assert(assert_cmd.clone()),
                                            loc: last_symbol_loc.clone(),
                                        });
                                        symbol_defs.push(InternalSymDefInfo::new(placement, b""));
                                    }
                                    ContentsCommand::Fill(fill) => {
                                        if let Ok(value) = evaluate_const(&fill.value)
                                            && let Ok(value) = u32::try_from(value)
                                        {
                                            let bytes = value.to_be_bytes();
                                            output_sections
                                                .section_infos
                                                .get_mut(primary_section_id)
                                                .fill = Some(bytes);
                                        }
                                    }
                                    ContentsCommand::OutputData(data) => {
                                        let width = data.width as u64;
                                        output_sections.script_output_data.push(
                                            crate::output_section_id::ScriptOutputData {
                                                section_id: primary_section_id,
                                                location_counter_index: inner_lc_idx,
                                                width: data.width as u8,
                                                value: data.value.clone(),
                                            },
                                        );
                                        location_counters.push(LocationCounter::Relative(
                                            Expression::Add(
                                                Box::new(Expression::LocationCounter),
                                                Box::new(Expression::Number(width)),
                                            ),
                                            last_symbol_loc.clone(),
                                            primary_section_id,
                                        ));
                                        last_symbol_loc = SymbolLoc::LocationCounter(
                                            inner_lc_idx,
                                            Some(primary_section_id),
                                        );
                                        inner_lc_idx += 1;
                                    }
                                }
                            }
                            if inner_lc_idx > inner_lc_start_idx {
                                let trailing_lc_info = SectionLocationInfo {
                                    location_counters: (inner_lc_start_idx, inner_lc_idx),
                                    location: None,
                                    at_location: None,
                                    at_region: None,
                                    is_top_level: false,
                                    overlay: None,
                                };
                                output_sections.add_secondary_section(
                                    primary_section_id,
                                    alignment::MIN,
                                    None,
                                    Some(trailing_lc_info),
                                    false,
                                );
                            }
                            last_lc_idx = inner_lc_idx;
                            section_start_lc_idx = last_lc_idx;
                        }
                        SectionCommand::SetLocation(new_location) => {
                            location_counters
                                .push(LocationCounter::Absolute(new_location.address.clone(), loc));
                            loc = SymbolLoc::LocationCounter(last_lc_idx, current_section_id);
                            if current_section_id.is_none() && self.num_location_counters == 0 {
                                output_sections.set_base_address(new_location.address.clone());
                                section_start_lc_idx = location_counters.len();
                            }
                            last_lc_idx += 1;
                        }
                        SectionCommand::Assert(assert_cmd) => {
                            let placement = SymbolPlacement::Redirect(Redirect {
                                kind: RedirectKind::Script,
                                expression: Expression::Assert(assert_cmd.clone()),
                                loc: loc.clone(),
                            });
                            symbol_defs.push(InternalSymDefInfo::new(placement, b""));
                        }
                        SectionCommand::Provide(provide) => {
                            let placement = SymbolPlacement::Redirect(Redirect {
                                kind: RedirectKind::Script,
                                expression: provide.value.clone(),
                                loc: loc.clone(),
                            });
                            symbol_defs.push(
                                InternalSymDefInfo::new(placement, provide.name)
                                    .with_provide()
                                    .with_hidden(provide.hidden),
                            );
                        }
                        SectionCommand::SymbolAssignment(assignment) => {
                            let placement = SymbolPlacement::Redirect(Redirect {
                                kind: RedirectKind::Script,
                                expression: assignment.expr.clone(),
                                loc: loc.clone(),
                            });
                            symbol_defs.push(InternalSymDefInfo::new(placement, assignment.name));
                        }
                        SectionCommand::Overlay(overlay) => {
                            let overlay_group = self.overlay_group;
                            self.overlay_group += 1;
                            let member_count = overlay.sections.len();
                            for (member, sec) in overlay.sections.iter().enumerate() {
                                let min_alignment =
                                    sec.alignment.unwrap_or(alignment::MIN).max(alignment::MIN);
                                let location_info = SectionLocationInfo {
                                    location_counters: (section_start_lc_idx, last_lc_idx),
                                    location: overlay.start_address.clone(),
                                    at_location: overlay
                                        .at_address
                                        .clone()
                                        .or(sec.at_address.clone()),
                                    at_region: overlay.at_region.or(sec.at_region),
                                    is_top_level: true,
                                    overlay: Some(crate::output_section_id::OverlayPlacement {
                                        group: overlay_group,
                                        member: member as u32,
                                        is_last: member + 1 == member_count,
                                    }),
                                };
                                let identity = P::section_identity_from_name(SectionName(
                                    sec.output_section_name,
                                ))
                                .with_context(|| {
                                    format!(
                                        "Output section `{}` cannot be identified from the name alone",
                                        SectionName(sec.output_section_name)
                                    )
                                })?;
                                let primary_section_id = output_sections.add_named_section(
                                    identity,
                                    min_alignment,
                                    sec.region.or(overlay.region),
                                    Some(&location_info),
                                    None,
                                    if sec.phdrs.is_empty() {
                                        overlay.phdrs.clone()
                                    } else {
                                        sec.phdrs.clone()
                                    },
                                    sec.attributes.as_ref(),
                                );
                                ordered_sections.push(primary_section_id);
                                current_section_id = Some(primary_section_id);
                                loc = SymbolLoc::SectionEnd(primary_section_id);
                                for contents_cmd in &sec.commands {
                                    if let ContentsCommand::Matcher(matcher) = contents_cmd {
                                        let input_order = matcher_uses_input_order(matcher);
                                        output_sections
                                            .set_input_order(primary_section_id, input_order);
                                        for pattern in &matcher.input_section_name_patterns {
                                            let output_info = SectionOutputInfo {
                                                section_id: primary_section_id,
                                                must_keep: matcher.must_keep,
                                                sorted: pattern.sort.needs_sort(),
                                                sort_by_init_priority: pattern.sort
                                                    == linker_script::SortKind::InitPriority,
                                                sort_by_alignment: pattern.sort
                                                    == linker_script::SortKind::Alignment,
                                                input_order,
                                            };
                                            let outcome = SectionRuleOutcome::section_rule_from_id::<
                                                P,
                                            >(
                                                primary_section_id, output_info
                                            );
                                            self.add_section_rule(
                                                SectionRule::new(
                                                    pattern.name,
                                                    matcher.input_file_pattern,
                                                    outcome,
                                                )?
                                                .with_excludes(&matcher.exclude_file_patterns)?,
                                            );
                                        }
                                    }
                                }
                                let ident: String =
                                    String::from_utf8_lossy(sec.output_section_name)
                                        .chars()
                                        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                                        .collect();
                                if !ident.is_empty() {
                                    let start_name =
                                        Box::leak(format!("__load_start_{ident}").into_boxed_str())
                                            .as_bytes();
                                    let stop_name =
                                        Box::leak(format!("__load_stop_{ident}").into_boxed_str())
                                            .as_bytes();
                                    symbol_defs.push(InternalSymDefInfo::new(
                                        SymbolPlacement::Redirect(Redirect {
                                            kind: RedirectKind::Script,
                                            expression: Expression::Loadaddr(
                                                sec.output_section_name,
                                            ),
                                            loc: SymbolLoc::SectionEnd(primary_section_id),
                                        }),
                                        start_name,
                                    ));
                                    symbol_defs.push(InternalSymDefInfo::new(
                                        SymbolPlacement::Redirect(Redirect {
                                            kind: RedirectKind::Script,
                                            expression: Expression::Add(
                                                Box::new(Expression::Loadaddr(
                                                    sec.output_section_name,
                                                )),
                                                Box::new(Expression::Sizeof(
                                                    sec.output_section_name,
                                                )),
                                            ),
                                            loc: SymbolLoc::SectionEnd(primary_section_id),
                                        }),
                                        stop_name,
                                    ));
                                }
                            }
                        }
                        SectionCommand::Include(_) => {
                            crate::bail!("INCLUDE inside SECTIONS was not expanded before layout");
                        }
                    }
                }
            } else if let linker_script::Command::Assert(assert_cmd) = cmd {
                let placement = SymbolPlacement::Redirect(Redirect {
                    kind: RedirectKind::Script,
                    expression: Expression::Assert(assert_cmd.clone()),
                    loc: loc_for_global_expr(&assert_cmd.expression, None),
                });
                symbol_defs.push(InternalSymDefInfo::new(placement, b""));
            } else if let linker_script::Command::Memory(regions) = cmd {
                memory_regions = regions.clone();
            } else if let linker_script::Command::Phdrs(phdrs) = cmd {
                program_headers = phdrs.clone();
            } else if let linker_script::Command::Include(_) = cmd {
                crate::bail!("INCLUDE was not expanded before layout");
            } else if let linker_script::Command::OutputFormat(output_format) = cmd {
                let target_format = match args.output_format_endian() {
                    Some(object::Endianness::Little) => {
                        output_format.little.unwrap_or(output_format.default)
                    }
                    Some(object::Endianness::Big) => {
                        output_format.big.unwrap_or(output_format.default)
                    }
                    None => output_format.default,
                };
                let target_arch = Architecture::parse_output_format(target_format);
                if target_arch == Architecture::Unsupported {
                    crate::bail!(
                        "{} is not yet supported",
                        String::from_utf8_lossy(target_format)
                    );
                }
                if args.architecture() != target_arch {
                    crate::bail!(
                        "Setting the output format using OUTPUT_FORMAT is currently unsupported"
                    );
                }
            } else if let linker_script::Command::OutputArch(arch) = cmd {
                let target_arch = Architecture::parse_output_arch(arch);
                if target_arch == Architecture::Unsupported {
                    crate::bail!("{} is not yet supported", String::from_utf8_lossy(arch));
                }
                if args.architecture() != target_arch {
                    crate::bail!(
                        "Setting the output architecture using OUTPUT_ARCH is currently unsupported"
                    );
                }
            }
        }

        self.num_location_counters += location_counters.len();

        Ok(ProcessedLinkerScript {
            symbol_defs,
            input: InputRef {
                file: input.input_file,
                data: input.script_bytes,
                entry: None,
            },
            memory_regions,
            program_headers,
            location_counters,
            ordered_sections,
        })
    }

    pub(crate) fn build<P: Platform>(mut self, args: &P::Args) -> LayoutRules<'data> {
        let section_rules = if self.rules.is_empty() {
            SectionRules::from_rules(&P::default_layout_rules(args))
        } else {
            P::linker_script_rules_pre_build(&mut self);
            SectionRules::from_rules(&self.rules)
        };

        LayoutRules { section_rules }
    }

    pub(crate) fn add_section_rule(&mut self, rule: SectionRule<'data>) {
        self.rules.push(rule);
    }
}

/// First matching linker-script rule for `.note.gnu.build-id` wins, matching GNU ld.
fn record_gnu_build_id_placement<P: Platform>(
    output_sections: &mut OutputSections<P>,
    rule: &SectionRule<'_>,
    placement: GnuBuildIdPlacement,
) {
    if output_sections.gnu_build_id_placement != GnuBuildIdPlacement::Builtin {
        return;
    }
    // Linker-generated notes have no input filename; `*` still matches an empty name.
    if rule.matches(NOTE_GNU_BUILD_ID_SECTION_NAME, Some(b"")) {
        output_sections.gnu_build_id_placement = placement;
    }
}
