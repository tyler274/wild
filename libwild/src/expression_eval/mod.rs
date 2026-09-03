//! Evaluation of linker script ASSERT commands and location-counter expressions.

mod early;
mod value;

use crate::error::Result;
use crate::layout;
use crate::layout::OutputRecordLayout;
use crate::linker_script::Expression;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::SymbolLoc;
#[allow(unused_imports)]
pub(crate) use early::*;
use hashbrown::HashMap;
#[allow(unused_imports)]
pub(crate) use value::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OsFileSystem;
    use crate::elf::Elf64;
    use crate::grouping::SequencedLinkerScript;
    use crate::input_data::FileId;
    use crate::layout::MemoryRegion;
    use crate::linker_script::AssertCommand;
    use crate::parsing::InternalSymDefInfo;
    use crate::parsing::ProcessedLinkerScript;
    use crate::parsing::Redirect;
    use crate::parsing::RedirectKind;
    use crate::parsing::SymbolPlacement;
    use crate::symbol_db::SymbolDb;
    use crate::symbol_db::SymbolIdRange;
    use colosseum::sync::Arena;

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
        let output_kind = crate::output_kind::OutputKind::PartialLink;
        let arena = Arena::new();
        let auxiliary =
            crate::input_data::AuxiliaryFiles::new(&args, &arena, &OsFileSystem).unwrap();
        let herd = Default::default();
        let mut symbol_db = SymbolDb::<Elf64>::new(&args, output_kind, &auxiliary, &herd).unwrap();
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
    fn test_align_zero_is_error() {
        assert!(eval_const(&Expression::Align(Box::new(Expression::Number(0)), None)).is_err());
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
                input: crate::input_data::InputRef {
                    file: crate::input_data::InputFileRef::for_testing(),
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
        memory_regions: &HashMap<&[u8], layout::MemoryRegion>,
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
