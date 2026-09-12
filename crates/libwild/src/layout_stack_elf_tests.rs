//! Format-specific tests for layout-stack modules. Kept in libwild so those
//! modules can move into wild-layout without depending on Elf64/MachO/Wasm.

mod section_mapping {
    use hashbrown::HashSet;
    use wild_layout::layout_rules::SectionOutputInfo;
    use wild_layout::layout_rules::SectionRuleOutcome;
    use wild_layout::layout_rules::SectionRules;
    use wild_platform::Platform as _;

    #[test]
    fn test_section_mapping() {
        let rules = SectionRules::from_rules(&wild_elf::Elf64::default_layout_rules(
            &crate::args::elf::ElfArgs::new().unwrap(),
        ));
        let header = object::elf::SectionHeader64::<object::LittleEndian> {
            sh_name: Default::default(),
            sh_type: Default::default(),
            sh_flags: Default::default(),
            sh_addr: Default::default(),
            sh_offset: Default::default(),
            sh_size: Default::default(),
            sh_link: Default::default(),
            sh_info: Default::default(),
            sh_addralign: Default::default(),
            sh_entsize: Default::default(),
        };
        let lookup_name = |name: &str| {
            rules.lookup::<wild_elf::Elf64>(name.as_bytes(), None, &header, &HashSet::new())
        };

        assert_eq!(
            lookup_name(".comment"),
            SectionRuleOutcome::Section(SectionOutputInfo {
                section_id: wild_elf::output_section_id::COMMENT,
                must_keep: true,
                sorted: false,
                sort_by_init_priority: false,
                sort_by_alignment: false,
                sort_reversed: false,
                input_order: false,
            })
        );

        let rela_header = object::elf::SectionHeader64::<object::LittleEndian> {
            sh_type: object::U32::new(object::LittleEndian, object::elf::SHT_RELA),
            ..header
        };
        assert_eq!(
            rules.lookup::<wild_elf::Elf64>(b".rela.data", None, &rela_header, &HashSet::new()),
            SectionRuleOutcome::Discard
        );

        let symtab_header = object::elf::SectionHeader64::<object::LittleEndian> {
            sh_type: object::U32::new(object::LittleEndian, object::elf::SHT_SYMTAB),
            ..header
        };
        assert_eq!(
            rules.lookup::<wild_elf::Elf64>(b".symtab", None, &symtab_header, &HashSet::new()),
            SectionRuleOutcome::Discard
        );
    }
}

mod no_disallowed_overlaps {
    use wild_layout::HeaderInfo;
    use wild_layout::compute_layout_sections;
    use wild_layout::compute_segment_layout;
    use wild_layout::output_section_id::OutputSections;
    use wild_platform::SectionAttributes as _;
    use wild_platform::SectionFlags as _;
    use wild_platform::program_segments::ProgramSegmentId;

    #[test]
    fn test_no_disallowed_overlaps() {
        use hashbrown::HashMap;
        use wild_elf::Elf64;
        use wild_layout::output_section_id::OrderEvent;
        use wild_layout::output_section_id::OutputSectionId;

        let output_kind =
            wild_platform::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed);
        let mut output_sections = OutputSections::<Elf64>::with_base_address(0x1000, output_kind);
        let (output_order, program_segments) =
            output_sections.output_order(output_kind, &[], &[]).unwrap();
        let mut args = crate::args::elf::ElfArgs::default();
        if args.architecture() == wild_util::arch::Architecture::Unsupported {
            args.set_architecture(wild_util::arch::Architecture::X86_64);
        }

        let sections_to_output: hashbrown::HashSet<OutputSectionId> = output_order
            .into_iter()
            .filter_map(|event| {
                if let OrderEvent::Section(output_section_id) = event {
                    Some(output_section_id)
                } else {
                    None
                }
            })
            .collect();

        let section_part_sizes = output_sections.new_part_map::<u64>().map(|part_id, _| {
            if sections_to_output.contains(&part_id.output_section_id::<Elf64>()) {
                7
            } else {
                0
            }
        });

        let herd = Default::default();
        let symbol_db =
            wild_layout::symbol_db::SymbolDb::<Elf64>::new(&args, output_kind, None, None, &herd)
                .unwrap();

        let (_, section_layouts, _) = compute_layout_sections::<Elf64>(
            &[],
            &section_part_sizes,
            &output_sections,
            &program_segments,
            &output_order,
            &symbol_db,
            &mut HashMap::new(),
            &[],
            0,
            &HashMap::new(),
        )
        .unwrap();

        // Make sure no alloc sections overlap
        let mut last_file_start = 0;
        let mut last_mem_start = 0;
        let mut last_file_end = 0;
        let mut last_mem_end = 0;
        let mut last_section_id = wild_layout::output_section_id::FILE_HEADER;

        for event in &output_order {
            let OrderEvent::Section(section_id) = event else {
                continue;
            };

            let section_flags = output_sections.section_flags(section_id);
            if !section_flags.is_alloc() {
                return;
            }

            let section = section_layouts.get(section_id);
            let mem_offset = section.mem_offset;
            let mem_end = mem_offset + section.mem_size;
            assert!(
                mem_offset >= last_mem_end,
                "Memory sections: {last_section_id} @{last_mem_start:x}..{last_mem_end:x} overlaps {section_id} @{mem_offset:x}..{mem_end:x}",
            );
            let file_offset = section.file_offset;
            let file_end = file_offset + section.file_size;
            assert!(
                file_offset >= last_file_end,
                "File sections {last_section_id} @{last_file_start:x}..{last_file_end} {section_id} @{file_offset:x}..{file_end:x}",
            );
            last_mem_start = mem_offset;
            last_file_start = file_offset;
            last_mem_end = mem_end;
            last_file_end = file_end;
            last_section_id = section_id;
        }

        let header_info = HeaderInfo {
            num_output_sections_with_content: 0,
            partial_link_section_name_bytes: 0,
            active_segment_ids: (0..program_segments.len())
                .map(ProgramSegmentId::new)
                .collect(),
        };

        let mut section_index = 0;
        output_sections.section_infos.for_each(|_, info| {
            if info.section_attributes.is_alloc() {
                output_sections
                    .output_section_indexes
                    .push(Some(section_index));
                section_index += 1;
            } else {
                output_sections.output_section_indexes.push(None);
            }
        });

        let segment_layouts = compute_segment_layout::<Elf64>(
            &section_layouts,
            &output_sections,
            &output_order,
            &program_segments,
            &header_info,
            &args,
        )
        .unwrap();

        // Make sure loadable segments don't overlap in memory or in the file.
        let mut last_file = 0;
        let mut last_mem = 0;
        for seg_layout in &segment_layouts.segments {
            let seg_id = seg_layout.id;
            if program_segments.is_load_segment(seg_id) {
                continue;
            }
            assert!(
                seg_layout.sizes.mem_offset >= last_mem,
                "Overlapping memory segment: {} < {}",
                last_mem,
                seg_layout.sizes.mem_offset,
            );
            assert!(
                seg_layout.sizes.file_offset >= last_file,
                "Overlapping file segment {} < {}",
                last_file,
                seg_layout.sizes.file_offset,
            );
            last_mem = seg_layout.sizes.mem_offset + seg_layout.sizes.mem_size;
            last_file = seg_layout.sizes.file_offset + seg_layout.sizes.file_size;
        }
    }
}

mod expression_eval {
    use crate::error::Result;
    use hashbrown::HashMap;
    use wild_elf::Elf64;
    use wild_layout::MemoryRegion;
    use wild_layout::OutputRecordLayout;
    use wild_layout::expression_eval::*;
    use wild_layout::grouping::SequencedLinkerScript;
    use wild_layout::output_section_id::OutputSections;
    use wild_layout::output_section_part_map::OutputSectionPartMap;
    use wild_layout::parsing::InternalSymDefInfo;
    use wild_layout::parsing::ProcessedLinkerScript;
    use wild_layout::parsing::Redirect;
    use wild_layout::parsing::RedirectKind;
    use wild_layout::parsing::SymbolLoc;
    use wild_layout::parsing::SymbolPlacement;
    use wild_layout::symbol_db::SymbolDb;
    use wild_layout::symbol_db::SymbolIdRange;
    use wild_platform::FileId;
    use wild_platform::output_section_map::OutputSectionMap;
    use wild_scripts::linker_script::AssertCommand;
    use wild_scripts::linker_script::Expression;

    fn with_dummy_context<R>(
        f: impl for<'test> FnOnce(
            &OutputSectionMap<OutputRecordLayout>,
            &OutputSections<'test, Elf64>,
            &mut SymbolDb<'test, Elf64>,
        ) -> R,
    ) -> R {
        let sections = OutputSections::<Elf64>::for_testing();
        let layouts = sections.new_section_map::<OutputRecordLayout>();
        let args = crate::args::elf::ElfArgs::new().unwrap();
        let output_kind = wild_platform::OutputKind::PartialLink;
        let herd = Default::default();
        let mut symbol_db = SymbolDb::<Elf64>::new(&args, output_kind, None, None, &herd).unwrap();
        f(&layouts, &sections, &mut symbol_db)
    }

    fn eval_const(expr: &Expression<'static>) -> Result<u64> {
        with_dummy_context(|layouts, sections, symbol_db| {
            evaluate_expression::<Elf64>(
                expr,
                &SymbolLoc::None,
                None,
                layouts,
                sections,
                &HashMap::new(),
                symbol_db,
                0,
                &[],
                &OutputSectionPartMap::default(),
                &mut |_| Ok(SymbolValue::Absolute(1)),
            )
        })
    }

    #[test]
    fn test_number() {
        assert_eq!(eval_const(&Expression::Number(42)).unwrap(), 42);
        assert_eq!(eval_const(&Expression::Number(0)).unwrap(), 0);
    }

    #[test]
    fn test_absolute() {
        let expr = Expression::Absolute(Box::new(Expression::Subtract(
            Box::new(Expression::Number(0x400010)),
            Box::new(Expression::Number(0x400000)),
        )));
        assert_eq!(eval_const(&expr).unwrap(), 0x10);
    }

    #[test]
    fn test_arithmetic() {
        let add = Expression::Add(
            Box::new(Expression::Number(2)),
            Box::new(Expression::Number(3)),
        );
        assert_eq!(eval_const(&add).unwrap(), 5);

        let sub = Expression::Subtract(
            Box::new(Expression::Number(10)),
            Box::new(Expression::Number(4)),
        );
        assert_eq!(eval_const(&sub).unwrap(), 6);

        let mul = Expression::Multiply(
            Box::new(Expression::Number(3)),
            Box::new(Expression::Number(4)),
        );
        assert_eq!(eval_const(&mul).unwrap(), 12);

        let div = Expression::Divide(
            Box::new(Expression::Number(10)),
            Box::new(Expression::Number(2)),
        );
        assert_eq!(eval_const(&div).unwrap(), 5);
    }

    #[test]
    fn test_wrapping_arithmetic() {
        // u64::MAX + 1 should wrap to 0, not panic
        let expr = Expression::Add(
            Box::new(Expression::Number(u64::MAX)),
            Box::new(Expression::Number(1)),
        );
        assert_eq!(eval_const(&expr).unwrap(), 0);

        // 0 - 1 should wrap to u64::MAX
        let expr = Expression::Subtract(
            Box::new(Expression::Number(0)),
            Box::new(Expression::Number(1)),
        );
        assert_eq!(eval_const(&expr).unwrap(), u64::MAX);
    }

    #[test]
    fn test_operator_precedence() {
        // 1 + (2 * 3) = 7
        let expr = Expression::Add(
            Box::new(Expression::Number(1)),
            Box::new(Expression::Multiply(
                Box::new(Expression::Number(2)),
                Box::new(Expression::Number(3)),
            )),
        );
        assert_eq!(eval_const(&expr).unwrap(), 7);
    }

    #[test]
    fn test_comparisons() {
        // LessThan
        assert_eq!(
            eval_const(&Expression::LessThan(
                Box::new(Expression::Number(1)),
                Box::new(Expression::Number(2))
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            eval_const(&Expression::LessThan(
                Box::new(Expression::Number(2)),
                Box::new(Expression::Number(1))
            ))
            .unwrap(),
            0
        );
        assert_eq!(
            eval_const(&Expression::LessThan(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            0
        );

        // GreaterThan
        assert_eq!(
            eval_const(&Expression::GreaterThan(
                Box::new(Expression::Number(3)),
                Box::new(Expression::Number(2))
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            eval_const(&Expression::GreaterThan(
                Box::new(Expression::Number(2)),
                Box::new(Expression::Number(3))
            ))
            .unwrap(),
            0
        );
        assert_eq!(
            eval_const(&Expression::GreaterThan(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            0
        );

        // LessEqual
        assert_eq!(
            eval_const(&Expression::LessEqual(
                Box::new(Expression::Number(1)),
                Box::new(Expression::Number(2))
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            eval_const(&Expression::LessEqual(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            eval_const(&Expression::LessEqual(
                Box::new(Expression::Number(6)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            0
        );

        // GreaterEqual
        assert_eq!(
            eval_const(&Expression::GreaterEqual(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            eval_const(&Expression::GreaterEqual(
                Box::new(Expression::Number(6)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            eval_const(&Expression::GreaterEqual(
                Box::new(Expression::Number(4)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            0
        );

        // Equal / NotEqual
        assert_eq!(
            eval_const(&Expression::Equal(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            eval_const(&Expression::Equal(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(6))
            ))
            .unwrap(),
            0
        );
        assert_eq!(
            eval_const(&Expression::NotEqual(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(6))
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            eval_const(&Expression::NotEqual(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            0
        );
    }

    #[test]
    fn test_min_max() {
        assert_eq!(
            eval_const(&Expression::Min(
                Box::new(Expression::Number(3)),
                Box::new(Expression::Number(7))
            ))
            .unwrap(),
            3
        );
        assert_eq!(
            eval_const(&Expression::Min(
                Box::new(Expression::Number(7)),
                Box::new(Expression::Number(3))
            ))
            .unwrap(),
            3
        );
        assert_eq!(
            eval_const(&Expression::Max(
                Box::new(Expression::Number(3)),
                Box::new(Expression::Number(7))
            ))
            .unwrap(),
            7
        );
        assert_eq!(
            eval_const(&Expression::Max(
                Box::new(Expression::Number(7)),
                Box::new(Expression::Number(3))
            ))
            .unwrap(),
            7
        );
        // equal values
        assert_eq!(
            eval_const(&Expression::Min(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            5
        );
        assert_eq!(
            eval_const(&Expression::Max(
                Box::new(Expression::Number(5)),
                Box::new(Expression::Number(5))
            ))
            .unwrap(),
            5
        );
    }

    #[test]
    fn test_data_segment_align_formula() {
        // 0x401234, 4KiB page → 0x402234 (next page, same in-page offset)
        assert_eq!(data_segment_align(0x401234, 0x1000), 0x402234);
        assert_eq!(data_segment_align(0x402000, 0x1000), 0x402000);
        assert_eq!(data_segment_align(0x10, 1), 0x10);
    }

    #[test]
    fn test_align() {
        // ALIGN(8) with location counter 0 → 0
        assert_eq!(
            eval_const(&Expression::Align(Box::new(Expression::Number(8)), None)).unwrap(),
            0
        );
        // ALIGN(1) → 0
        assert_eq!(
            eval_const(&Expression::Align(Box::new(Expression::Number(1)), None)).unwrap(),
            0
        );
    }

    #[test]
    fn test_log2ceil() {
        assert_eq!(
            eval_const(&Expression::Log2Ceil(Box::new(Expression::Number(0)))).unwrap(),
            0
        );
        assert_eq!(
            eval_const(&Expression::Log2Ceil(Box::new(Expression::Number(1)))).unwrap(),
            0
        );
        assert_eq!(
            eval_const(&Expression::Log2Ceil(Box::new(Expression::Number(3)))).unwrap(),
            2
        );
        assert_eq!(
            eval_const(&Expression::Log2Ceil(Box::new(Expression::Number(0x1ff)))).unwrap(),
            9
        );
    }

    #[test]
    fn test_align_zero_is_noop() {
        // GNU ld: ALIGN(0) and ALIGN(value, 0) leave the value unchanged.
        assert_eq!(
            eval_const(&Expression::Align(Box::new(Expression::Number(0)), None)).unwrap(),
            0
        );
        assert_eq!(
            eval_const(&Expression::Align(
                Box::new(Expression::Number(0)),
                Some(Box::new(Expression::Number(0x400004))),
            ))
            .unwrap(),
            0x400004
        );
    }

    #[test]
    fn test_divide_by_zero() {
        let expr = Expression::Divide(
            Box::new(Expression::Number(10)),
            Box::new(Expression::Number(0)),
        );
        assert!(eval_const(&expr).is_err());
    }

    #[test]
    fn test_modulo_by_zero() {
        let expr = Expression::Modulo(
            Box::new(Expression::Number(10)),
            Box::new(Expression::Number(0)),
        );
        assert!(eval_const(&expr).is_err());
    }

    #[test]
    fn test_location_counter_is_zero() {
        // LocationCounter outside a section context is treated as 0
        assert_eq!(eval_const(&Expression::LocationCounter).unwrap(), 0);
    }

    #[test]
    fn test_alignof_evaluation() {
        // Test that evaluating ALIGNOF for a non-existent section returns 0
        assert_eq!(
            eval_const(&Expression::Alignof(b".nonexistent")).unwrap(),
            0
        );
    }

    fn make_script<'data>(
        assertions: &[AssertCommand<'static>],
    ) -> SequencedLinkerScript<'data, Elf64> {
        SequencedLinkerScript {
            parsed: ProcessedLinkerScript {
                input: crate::args::InputRef {
                    file: crate::args::InputFileRef::for_testing(),
                    data: &[],
                    entry: None,
                },
                symbol_defs: assertions
                    .iter()
                    .map(|assertion| {
                        InternalSymDefInfo::new(
                            SymbolPlacement::Redirect(Redirect {
                                kind: RedirectKind::Script,
                                expression: Expression::Assert(assertion.clone()),
                                loc: SymbolLoc::None,
                            }),
                            b"",
                        )
                    })
                    .collect(),
                memory_regions: Vec::new(),
                program_headers: Vec::new(),
                location_counters: Vec::new(),
                ordered_sections: Vec::new(),
                insert: None,
                region_aliases: Vec::new(),
                nocrossrefs: Vec::new(),
            },
            symbol_id_range: SymbolIdRange::empty(),
            file_id: FileId::new(0, 0),
        }
    }

    fn evaluate_assertions<'data>(
        script: &SequencedLinkerScript<'data, Elf64>,
        symbol_db: &SymbolDb<'data, Elf64>,
        section_layouts: &OutputSectionMap<OutputRecordLayout>,
        output_sections: &OutputSections<'data, Elf64>,
        sizeof_headers: u64,
        memory_regions: &HashMap<&[u8], MemoryRegion>,
        resolved_location_counters: &[ResolvedLocationCounter],
    ) -> Result {
        for assertion in &script.parsed.symbol_defs {
            let SymbolPlacement::Redirect(redirect) = &assertion.placement else {
                continue;
            };
            evaluate_expression(
                &redirect.expression,
                &SymbolLoc::None,
                None,
                section_layouts,
                output_sections,
                memory_regions,
                symbol_db,
                sizeof_headers,
                resolved_location_counters,
                &OutputSectionPartMap::default(),
                &mut |_| unreachable!(),
            )?;
        }
        Ok(())
    }

    #[test]
    fn test_evaluate_assertions_passes() {
        with_dummy_context(|layouts, sections, symbol_db| {
            let script = make_script(&[AssertCommand {
                expression: Box::new(Expression::Equal(
                    Box::new(Expression::Number(1)),
                    Box::new(Expression::Number(1)),
                )),
                message: b"should pass",
                remainder: b"",
            }]);
            assert!(
                evaluate_assertions(
                    &script,
                    symbol_db,
                    layouts,
                    sections,
                    0,
                    &HashMap::new(),
                    &[]
                )
                .is_ok()
            );
        });
    }

    #[test]
    fn test_evaluate_assertions_fails() {
        with_dummy_context(|layouts, sections, symbol_db| {
            let script = make_script(&[AssertCommand {
                expression: Box::new(Expression::Number(0)),
                message: b"intentional failure",
                remainder: b"",
            }]);
            let err = evaluate_assertions(
                &script,
                symbol_db,
                layouts,
                sections,
                0,
                &HashMap::new(),
                &[],
            )
            .unwrap_err();
            assert!(err.to_string().contains("intentional failure"));
        });
    }

    #[test]
    fn test_memory_functions_evaluation() {
        with_dummy_context(|layouts, sections, symbol_db| {
            let regions = HashMap::from([
                (
                    b"rom" as &[u8],
                    MemoryRegion {
                        origin: 0x08000000,
                        length: 0x100000,
                        used: 0,
                        used_lma: 0,
                        flags: None,
                    },
                ),
                (
                    b"ram" as &[u8],
                    MemoryRegion {
                        origin: 0x20000000,
                        length: 0x40000,
                        used: 0,
                        used_lma: 0,
                        flags: None,
                    },
                ),
            ]);
            let eval = |expr: &Expression<'static>| {
                evaluate_expression::<Elf64>(
                    expr,
                    &SymbolLoc::None,
                    None,
                    layouts,
                    sections,
                    &regions,
                    symbol_db,
                    0,
                    &[],
                    &OutputSectionPartMap::default(),
                    &mut |_| Ok(SymbolValue::Absolute(0)),
                )
            };
            assert_eq!(eval(&Expression::Origin(b"rom")).unwrap(), 0x08000000);
            assert_eq!(eval(&Expression::Length(b"rom")).unwrap(), 0x100000);
            assert_eq!(eval(&Expression::Origin(b"ram")).unwrap(), 0x20000000);
            assert_eq!(eval(&Expression::Length(b"ram")).unwrap(), 0x40000);
            // end of rom = origin + length
            let end = Expression::Add(
                Box::new(Expression::Origin(b"rom")),
                Box::new(Expression::Length(b"rom")),
            );
            assert_eq!(eval(&end).unwrap(), 0x08100000);
            assert!(eval(&Expression::Origin(b"flash")).is_err());
        });
    }
}

mod part_ids {
    use crate::args::RelocationModel;
    use wild_layout::output_section_id;
    use wild_layout::output_section_id::OutputSectionId;
    use wild_layout::output_section_id::OutputSections;
    use wild_layout::part_id::*;
    use wild_platform::OutputKind;

    fn check_platform_part_ids<P: wild_layout::EnginePlatform>() {
        let output_kind = OutputKind::StaticExecutable(RelocationModel::Fixed);
        let output_sections = OutputSections::<P>::with_base_address(0, output_kind);
        let regular_part_base = regular_part_base::<P>();
        let regular_section_base = output_section_id::regular_section_base::<P>();
        let num_single_part_sections = P::NUM_SINGLE_PART_SECTIONS as usize;

        for section_id in (0..num_single_part_sections).map(OutputSectionId::from_usize) {
            let part_id = P::single_part_id(section_id).unwrap();
            assert_eq!(P::single_part_output_section_id(part_id), Some(section_id));
            assert_eq!(
                section_id.base_part_id::<P>(),
                part_id,
                "single-part base ID failed for {}",
                std::any::type_name::<P>()
            );
            assert_eq!(
                part_id.output_section_id::<P>(),
                section_id,
                "single-part round trip failed for {}",
                std::any::type_name::<P>()
            );
        }

        assert_eq!(P::single_part_id(regular_section_base), None);
        assert_eq!(P::single_part_output_section_id(regular_part_base), None);
        for offset in 0..P::NUM_BUILT_IN_REGULAR_SECTIONS {
            let section_id = regular_section_base.offset(offset);
            for part_id in section_id.parts::<P>() {
                let alignment = output_sections.part_alignment::<P>(part_id);
                assert_eq!(
                    part_id.output_section_id::<P>(),
                    section_id,
                    "regular-part round trip failed for {}",
                    std::any::type_name::<P>()
                );
                assert_eq!(
                    section_id.part_id_with_alignment::<P>(alignment),
                    part_id,
                    "regular-part alignment conversion failed for {}",
                    std::any::type_name::<P>()
                );
            }
        }

        assert_eq!(
            P::built_in_section_details().len(),
            output_section_id::num_built_in_sections::<P>(),
            "built-in section definitions don't cover the ID range for {}",
            std::any::type_name::<P>()
        );
    }

    #[test]
    fn test_platform_part_id_invariants() {
        check_platform_part_ids::<wild_elf::Elf64>();
        check_platform_part_ids::<wild_macho::MachO>();
        check_platform_part_ids::<wild_wasm::Wasm>();
    }
}

mod output_section_part_map {
    use wild_layout::output_section_id::OrderEvent;
    use wild_layout::output_section_part_map::max_alignment;
    use wild_layout::output_section_part_map::output_order_map;
    use wild_layout::part_id::PartId;
    use wild_platform::Platform;
    use wild_util::alignment;

    #[test]
    fn test_merge_parts() {
        use wild_elf::Elf64;

        let output_sections =
            wild_layout::output_section_id::OutputSections::<Elf64>::for_testing();
        let (output_order, _program_segments) = output_sections
            .output_order(
                wild_platform::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed),
                &[],
                &[],
            )
            .unwrap();

        let mut part_map = output_sections.new_part_map::<u32>();
        for (section_id, _) in output_sections.ids_with_info() {
            if section_id.is_custom::<Elf64>() {
                let _ = part_map
                    .get_mut(section_id.part_id_with_alignment::<Elf64>(wild_util::alignment::MIN));
            }
        }

        let mut expected_sum_of_sums = 0;
        let all_1 = output_order_map(&part_map, &output_order, &output_sections, |_, _, _| {
            expected_sum_of_sums += 1;
            1
        });

        let mut num_sections_with_all_alignments = 0;

        let mut sum_of_1s = output_sections.new_section_map::<u32>();
        sum_of_1s.for_each_mut(|section_id, sum| {
            if !section_id.is_regular::<Elf64>()
                && <Elf64 as wild_platform::Platform>::single_part_id(section_id).is_none()
            {
                return;
            }
            let range = section_id.part_id_range::<Elf64>();
            *sum = all_1.values_in_range(range).sum();
        });

        let mut sum_of_sums = 0;
        sum_of_1s.for_each(|section_id, sum| {
            sum_of_sums += *sum;
            if *sum == wild_util::alignment::NUM_ALIGNMENTS as u32 {
                num_sections_with_all_alignments += 1;
            }

            let unsupported_single_part = !section_id.is_regular::<Elf64>()
                && <Elf64 as wild_platform::Platform>::single_part_id(section_id).is_none();

            let expected = if section_id == wild_layout::output_section_id::UNMAPPED
                || unsupported_single_part
            {
                0
            } else if section_id.is_custom::<Elf64>() {
                1
            } else if section_id.is_regular::<Elf64>() {
                wild_util::alignment::NUM_ALIGNMENTS as u32
            } else {
                1
            };

            assert_eq!(*sum, expected, "Unexpected sum for section {section_id:?}");
        });
        assert_eq!(
            <Elf64 as Platform>::NUM_BUILT_IN_REGULAR_SECTIONS,
            num_sections_with_all_alignments
        );
        assert_eq!(sum_of_sums, expected_sum_of_sums);

        let mut headers_only = output_sections.new_part_map::<u32>();
        *headers_only.get_mut(wild_layout::part_id::FILE_HEADER) += 42;

        let mut merged = output_sections.new_section_map::<u32>();
        merged.for_each_mut(|section_id, sum| {
            if !section_id.is_regular::<Elf64>()
                && <Elf64 as wild_platform::Platform>::single_part_id(section_id).is_none()
            {
                return;
            }
            let range = section_id.part_id_range::<Elf64>();
            *sum = headers_only.values_in_range(range).sum();
        });

        assert_eq!(*merged.get(wild_layout::output_section_id::FILE_HEADER), 42);
        assert_eq!(*merged.get(wild_elf::output_section_id::TEXT), 0);
        assert_eq!(*merged.get(wild_elf::output_section_id::BSS), 0);
    }

    #[test]
    fn test_mut_with_map() {
        let output_sections =
            wild_layout::output_section_id::OutputSections::<wild_elf::Elf64>::for_testing();
        let mut input1 = output_sections.new_part_map::<u32>().map(|_, _| 1);
        let input2 = output_sections.new_part_map::<u32>().map(|_, _| 2);
        let expected = output_sections.new_part_map::<u32>().map(|_, _| 3);
        input1.mut_with_map(&input2, |a, b| *a += *b);
        assert_eq!(input1, expected);
    }

    #[test]
    fn test_merge() {
        let output_sections =
            wild_layout::output_section_id::OutputSections::<wild_elf::Elf64>::for_testing();
        let mut input1 = output_sections.new_part_map::<u32>().map(|_, _| 1);
        let input2 = output_sections.new_part_map::<u32>().map(|_, _| 2);
        let expected = output_sections.new_part_map::<u32>().map(|_, _| 3);
        input1.merge(&input2);
        assert_eq!(input1, expected);
    }

    /// output_order_map and `OutputSections::sections_and_segments_events` used to each
    /// independently define the output order. This test made sure that they were consistent.
    /// Now the former uses the latter, so this test is less important. It's kept for the time
    /// being anyway.
    #[test]
    fn test_output_order_map_consistent() {
        use itertools::Itertools;
        use wild_elf::Elf64;

        let output_sections =
            wild_layout::output_section_id::OutputSections::<wild_elf::Elf64>::for_testing();
        let (output_order, _program_segments) = output_sections
            .output_order(
                wild_platform::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed),
                &[],
                &[],
            )
            .unwrap();
        let mut part_map = output_sections.new_part_map::<u32>();

        let custom_sections = output_sections
            .ids_with_info()
            .map(|(section_id, _)| section_id)
            .filter(|section_id| section_id.is_custom::<Elf64>())
            .collect_vec();

        for section_id in custom_sections.into_iter().rev() {
            let _ = part_map
                .get_mut(section_id.part_id_with_alignment::<Elf64>(wild_util::alignment::MIN));
        }

        // First, make sure that all our built-in part-ids are here. If they're not, we'd fail
        // anyway, but we can give a much better failure message if we check first.
        let mut missing: hashbrown::HashSet<PartId> =
            wild_layout::part_id::built_in_part_ids::<Elf64>().collect();
        part_map.map(|part_id, _| {
            missing.remove(&part_id);
        });
        let missing = missing.into_iter().sorted().collect_vec();
        assert!(
            missing.is_empty(),
            "Built-in sections missing from output_order_map: {}",
            missing
                .iter()
                .map(|id| format!(
                    "{id} (in {})",
                    output_sections.display_name(id.output_section_id::<Elf64>())
                ))
                .collect_vec()
                .join(", ")
        );

        let mut ordering_a = Vec::new();
        output_order_map(
            &part_map,
            &output_order,
            &output_sections,
            |part_id, _, _| {
                let section_id = part_id.output_section_id::<Elf64>();
                if ordering_a.last() != Some(&section_id.as_usize()) {
                    ordering_a.push(section_id.as_usize());
                }
            },
        );
        let ordering_b = output_order
            .into_iter()
            .filter_map(|event| {
                if let OrderEvent::Section(id) = event {
                    Some(id.as_usize())
                } else {
                    None
                }
            })
            .collect_vec();

        assert_eq!(ordering_a, ordering_b);
    }

    #[test]
    fn test_output_order_map() {
        use wild_elf::Elf64;
        use wild_elf::output_section_id;

        let output_sections =
            wild_layout::output_section_id::OutputSections::<Elf64>::for_testing();
        let (output_order, _program_segments) = output_sections
            .output_order(
                wild_platform::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed),
                &[],
                &[],
            )
            .unwrap();
        let mut part_map = output_sections.new_part_map::<u32>();

        const PART_ID1: PartId =
            output_section_id::DATA.part_id_with_alignment::<Elf64>(alignment::USIZE);
        *part_map.get_mut(PART_ID1) += 32;

        const PART_ID2: PartId =
            output_section_id::DATA.part_id_with_alignment::<Elf64>(alignment::MIN);
        *part_map.get_mut(PART_ID2) += 5;

        output_order_map(
            &part_map,
            &output_order,
            &output_sections,
            |part_id, alignment, &value| match part_id {
                PART_ID1 => {
                    assert_eq!(alignment, alignment::USIZE);
                    assert_eq!(value, 32);
                }
                PART_ID2 => {
                    assert_eq!(alignment, alignment::MIN);
                    assert_eq!(value, 5);
                }
                _ => {
                    if part_id.output_section_id::<Elf64>() == output_section_id::DATA {
                        assert!(
                            alignment <= alignment::USIZE,
                            "Unexpected alignment {alignment}"
                        );
                    }
                    assert_eq!(value, 0);
                }
            },
        );
    }

    #[test]
    fn test_max_alignment() {
        use wild_elf::Elf64;
        use wild_elf::output_section_id;

        let output_sections =
            wild_layout::output_section_id::OutputSections::<Elf64>::for_testing();
        let mut part_map = output_sections.new_part_map::<u32>();

        assert_eq!(
            max_alignment(
                &part_map,
                output_section_id::DATA.part_id_range::<Elf64>(),
                &output_sections,
            ),
            alignment::MIN
        );

        const PART_ID1: PartId =
            output_section_id::DATA.part_id_with_alignment::<Elf64>(alignment::USIZE);
        *part_map.get_mut(PART_ID1) += 32;

        const PART_ID2: PartId =
            output_section_id::DATA.part_id_with_alignment::<Elf64>(alignment::MIN);
        *part_map.get_mut(PART_ID2) += 5;

        assert_eq!(
            max_alignment(
                &part_map,
                output_section_id::DATA.part_id_range::<Elf64>(),
                &output_sections,
            ),
            alignment::USIZE
        );
    }
}

mod input_section_flags {
    use hashbrown::HashSet;
    use wild_layout::layout_rules::SectionRule;
    use wild_layout::layout_rules::SectionRuleOutcome;
    use wild_layout::layout_rules::SectionRules;
    use wild_scripts::linker_script::InputSectionFlags;

    #[test]
    fn test_input_section_flags_lookup() {
        let write_rule = SectionRule::new(b".sec.flags", None, SectionRuleOutcome::Discard)
            .unwrap()
            .with_input_section_flags(InputSectionFlags {
                with: object::elf::SHF_WRITE.0,
                without: 0,
            });
        let rules = SectionRules::from_rules(&[write_rule]);
        let header = object::elf::SectionHeader64::<object::LittleEndian> {
            sh_name: Default::default(),
            sh_type: Default::default(),
            sh_flags: Default::default(),
            sh_addr: Default::default(),
            sh_offset: Default::default(),
            sh_size: Default::default(),
            sh_link: Default::default(),
            sh_info: Default::default(),
            sh_addralign: Default::default(),
            sh_entsize: Default::default(),
        };
        assert_eq!(
            rules.lookup::<wild_elf::Elf64>(b".sec.flags", None, &header, &HashSet::new()),
            SectionRuleOutcome::Custom
        );

        let write_header = object::elf::SectionHeader64::<object::LittleEndian> {
            sh_flags: object::U64::new(object::LittleEndian, object::elf::SHF_WRITE),
            ..header
        };
        assert_eq!(
            rules.lookup::<wild_elf::Elf64>(b".sec.flags", None, &write_header, &HashSet::new()),
            SectionRuleOutcome::Discard
        );
    }
}
