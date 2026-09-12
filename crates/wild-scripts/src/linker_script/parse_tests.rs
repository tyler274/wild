use super::*;
use crate::inputs::InputSpec;
use crate::linker_script::ContentsCommand;
use crate::linker_script::InputSectionFlags;
use crate::linker_script::Location;
use crate::linker_script::Matcher;
use crate::linker_script::OnlyIf;
use crate::linker_script::OutputData;
use crate::linker_script::OutputDataWidth;
use crate::linker_script::RelocatableAnchor;
use crate::linker_script::Section;
use crate::linker_script::SectionAttributes;
use crate::linker_script::SectionCommand;
use crate::linker_script::SectionPattern;
use crate::linker_script::Sections;
use crate::linker_script::SortKind;
use crate::linker_script::SymbolAssignment;
use crate::linker_script::maybe_apply_sysroot;
use itertools::assert_equal;
use std::assert_matches;
use std::path::Path;
use wild_util::alignment::Alignment;

fn parse_script(text: &str) -> Result<LinkerScript<'_>> {
    LinkerScript::parse(text.as_bytes(), Path::new("test-linker-script.txt"))
}

fn inputs_from_script(text: &str) -> Result<Vec<Input>> {
    let script = parse_script(text)?;
    let mut inputs = Vec::new();
    foreach_input(&script.commands, Modifiers::default(), &mut |input| {
        inputs.push(input);
        Ok(())
    })?;
    Ok(inputs)
}

#[test]
fn test_inputs_from_script() {
    let inputs = inputs_from_script(
        r"/* GNU ld script */
            GROUP ( libgcc_s.so.1 -lgcc )
        ",
    )
    .unwrap();
    assert_equal(
        inputs.into_iter().map(|i| i.spec),
        [
            InputSpec::File(Box::from(Path::new("libgcc_s.so.1"))),
            InputSpec::Lib(Box::from("gcc")),
        ],
    );

    let inputs = inputs_from_script("INPUT(\"libbar.so\")").unwrap();
    assert_equal(
        inputs.into_iter().map(|i| i.spec),
        [InputSpec::File(Box::from(Path::new("libbar.so")))],
    );

    let inputs = inputs_from_script("INPUT(libfoo.so)").unwrap();
    assert_equal(
        inputs.into_iter().map(|i| i.spec),
        [InputSpec::File(Box::from(Path::new("libfoo.so")))],
    );
}

#[test]
fn test_test_inputs_from_script() {
    let inputs = inputs_from_script(
            r"OUTPUT_FORMAT(elf64-x86-64)
            GROUP ( /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc_nonshared.a  AS_NEEDED ( /lib64/ld-linux-x86-64.so.2 ) )
        ",
        )
        .unwrap();
    assert_equal(
        inputs.into_iter().map(|i| i.spec),
        [
            InputSpec::File(Box::from(Path::new("/lib/x86_64-linux-gnu/libc.so.6"))),
            InputSpec::File(Box::from(Path::new(
                "/usr/lib/x86_64-linux-gnu/libc_nonshared.a",
            ))),
            InputSpec::File(Box::from(Path::new("/lib64/ld-linux-x86-64.so.2"))),
        ],
    );
}

#[test]
fn test_sysroot_application() {
    let sysroot = Path::new("/usr/aarch64-linux-gnu");
    // Linker script is located in the sysroot
    assert_equal(
        maybe_apply_sysroot(
            &sysroot.join("lib/libc.so"),
            Path::new("/lib/libc.so.6"),
            sysroot,
        ),
        Some(Box::from(sysroot.join("lib/libc.so.6"))),
    );
    // Linker script is not located in the sysroot
    assert_equal(
        maybe_apply_sysroot(
            Path::new("/lib/libc.so"),
            Path::new("/lib/libc.so.6"),
            sysroot,
        ),
        None,
    );
    // Sysroot enforced by `=/`
    assert_equal(
        maybe_apply_sysroot(
            Path::new("/lib/libc.so"),
            Path::new("=/lib/libc.so.6"),
            sysroot,
        ),
        Some(Box::from(sysroot.join("lib/libc.so.6"))),
    );
    // Sysroot enforced by `=`
    assert_equal(
        maybe_apply_sysroot(
            Path::new("/lib/libc.so"),
            Path::new("=lib/libc.so.6"),
            sysroot,
        ),
        Some(Box::from(sysroot.join("lib/libc.so.6"))),
    );
    // Sysroot enforced by `$SYSROOT`
    assert_equal(
        maybe_apply_sysroot(
            Path::new("/lib/libc.so"),
            Path::new("$SYSROOT/lib/libc.so.6"),
            sysroot,
        ),
        Some(Box::from(sysroot.join("lib/libc.so.6"))),
    );
    // Sysroot enforced by `$SYSROOT`
    assert_equal(
        maybe_apply_sysroot(
            Path::new("/lib/libc.so"),
            Path::new("$SYSROOTlib/libc.so.6"),
            sysroot,
        ),
        Some(Box::from(sysroot.join("lib/libc.so.6"))),
    );
}

#[track_caller]
fn check_section_command(input: &str, expected: &SectionCommand) {
    match parse_section_command.parse(BStr::new(input)) {
        Ok(actual) => assert_eq!(&actual, expected),
        Err(e) => panic!("Parse failed:\n{e}"),
    }
}

#[test]
fn test_section_command() {
    check_section_command(
        ".text : { *(.text .text2) *(.text3) }",
        &SectionCommand::Section(Section {
            output_section_name: b".text",
            commands: vec![
                ContentsCommand::Matcher(Matcher {
                    must_keep: false,
                    input_file_pattern: None,
                    exclude_file_patterns: vec![],
                    input_section_flags: InputSectionFlags::EMPTY,
                    input_section_name_patterns: vec![
                        SectionPattern {
                            name: b".text",
                            sort: SortKind::None,
                            reversed: false,
                        },
                        SectionPattern {
                            name: b".text2",
                            sort: SortKind::None,
                            reversed: false,
                        },
                    ],
                }),
                ContentsCommand::Matcher(Matcher {
                    must_keep: false,
                    input_file_pattern: None,
                    exclude_file_patterns: vec![],
                    input_section_flags: InputSectionFlags::EMPTY,
                    input_section_name_patterns: vec![SectionPattern {
                        name: b".text3",
                        sort: SortKind::None,
                        reversed: false,
                    }],
                }),
            ],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_section_command_with_start_address_expression() {
    check_section_command(
        "__ksymtab 0 : ALIGN(8) { *(___ksymtab+) }",
        &SectionCommand::Section(Section {
            output_section_name: b"__ksymtab",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: false,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b"___ksymtab+",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: Some(Alignment::new(8).unwrap()),
            align_with_input: false,
            subalign: None,
            start_address_expression: Some(Expression::Number(0)),
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_section_command_with_subalign() {
    check_section_command(
        ".s : ALIGN(32) SUBALIGN(4) { *(.s) }",
        &SectionCommand::Section(Section {
            output_section_name: b".s",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: false,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".s",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: Some(Alignment::new(32).unwrap()),
            align_with_input: false,
            subalign: Some(Alignment::new(4).unwrap()),
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_section_command_with_subalign_only() {
    check_section_command(
        ".s : SUBALIGN(4) { *(.s) }",
        &SectionCommand::Section(Section {
            output_section_name: b".s",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: false,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".s",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: false,
            subalign: Some(Alignment::new(4).unwrap()),
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_section_command_with_align_start_address() {
    check_section_command(
        ".data ALIGN(0x2000) : { *(.data) }",
        &SectionCommand::Section(Section {
            output_section_name: b".data",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: false,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".data",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: Some(Expression::Align(
                Box::new(Expression::Number(0x2000)),
                None,
            )),
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_section_command_with_type_attribute() {
    check_section_command(
        ".note (TYPE = SHT_NOTE) : { BYTE(1) }",
        &SectionCommand::Section(Section {
            output_section_name: b".note",
            commands: vec![ContentsCommand::OutputData(OutputData {
                width: OutputDataWidth::Byte,
                value: Expression::Number(1),
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: Some(SectionAttributes::Type(object::elf::SHT_NOTE.0)),
            only_if: None,
        }),
    );
    check_section_command(
        ".note (TYPE=7) : { BYTE(1) }",
        &SectionCommand::Section(Section {
            output_section_name: b".note",
            commands: vec![ContentsCommand::OutputData(OutputData {
                width: OutputDataWidth::Byte,
                value: Expression::Number(1),
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: Some(SectionAttributes::Type(7)),
            only_if: None,
        }),
    );
    check_section_command(
        ".ro (READONLY (TYPE = SHT_NOTE)) : { BYTE(1) }",
        &SectionCommand::Section(Section {
            output_section_name: b".ro",
            commands: vec![ContentsCommand::OutputData(OutputData {
                width: OutputDataWidth::Byte,
                value: Expression::Number(1),
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: Some(SectionAttributes::ReadonlyType(object::elf::SHT_NOTE.0)),
            only_if: None,
        }),
    );
    check_section_command(
        ".bss (TYPE = SHT_NOBITS) : { *(.bss) }",
        &SectionCommand::Section(Section {
            output_section_name: b".bss",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: false,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".bss",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: Some(SectionAttributes::Type(object::elf::SHT_NOBITS.0)),
            only_if: None,
        }),
    );
}

#[test]
fn test_section_command_rejects_unknown_type_name() {
    assert!(
        parse_section_command
            .parse(BStr::new(".foo (TYPE = SHT_FOO) : { BYTE(1) }"))
            .is_err()
    );
}

#[track_caller]
fn check_linker_script(input: &str, expected: &LinkerScript) {
    let actual = parse_script(input).unwrap();
    assert_eq!(&actual, expected);
}

#[test]
fn test_basic_linker_script() {
    check_linker_script(
        r"
            ENTRY(_start)
            SECTIONS {
                . = 0x1000000;
                . = ALIGN(16);
                .foo : ALIGN(8) {
                    start_foo = .;
                    KEEP(*(.rodata.foo));
                    . = ALIGN(32);
                    end_foo = .;
                }
            }
        ",
        &LinkerScript {
            commands: vec![
                Command::Entry(b"_start"),
                Command::Sections(Sections {
                    commands: vec![
                        SectionCommand::SetLocation(Location {
                            address: Expression::Number(0x1000000),
                        }),
                        SectionCommand::SetLocation(Location {
                            address: Expression::Align(Box::new(Expression::Number(16)), None),
                        }),
                        SectionCommand::Section(Section {
                            output_section_name: b".foo",
                            commands: vec![
                                ContentsCommand::SymbolAssignment(SymbolAssignment {
                                    name: b"start_foo",
                                    expr: Expression::LocationCounter,
                                    hidden: false,
                                }),
                                ContentsCommand::Matcher(Matcher {
                                    must_keep: true,
                                    input_file_pattern: None,
                                    exclude_file_patterns: vec![],
                                    input_section_flags: InputSectionFlags::EMPTY,
                                    input_section_name_patterns: vec![SectionPattern {
                                        name: b".rodata.foo",
                                        sort: SortKind::None,
                                        reversed: false,
                                    }],
                                }),
                                ContentsCommand::SetLocation(Location {
                                    address: Expression::Align(
                                        Box::new(Expression::Number(32)),
                                        None,
                                    ),
                                }),
                                ContentsCommand::SymbolAssignment(SymbolAssignment {
                                    name: b"end_foo",
                                    expr: Expression::LocationCounter,
                                    hidden: false,
                                }),
                            ],
                            alignment: Some(Alignment::new(8).unwrap()),
                            align_with_input: false,
                            subalign: None,
                            start_address_expression: None,
                            phdrs: vec![],
                            at_address: None,
                            region: None,
                            at_region: None,
                            fill: None,
                            attributes: None,
                            only_if: None,
                        }),
                    ],
                }),
            ],
        },
    );
}

#[test]
fn test_version_command() {
    let script = parse_script(
        r"
            VERSION {
                VERS_1.0 {
                    global: foo; bar*;
                    local: *;
                };
            }
            ",
    )
    .unwrap();

    let version_content = script.get_version_script_content().unwrap();
    let version_str = std::str::from_utf8(version_content).unwrap().trim();

    assert!(version_str.contains("VERS_1.0"));
    assert!(version_str.contains("global:"));
    assert!(version_str.contains("foo"));
    assert!(version_str.contains("bar*"));
    assert!(version_str.contains("local:"));
}

#[test]
fn test_version_command_with_nested_braces() {
    let script = parse_script(
        r#"
            VERSION {
                VERS_1.0 {
                    global: 
                        extern "C++" {
                            ns::*;
                        };
                };
            }
            "#,
    )
    .unwrap();

    let version_content = script.get_version_script_content().unwrap();
    let version_str = std::str::from_utf8(version_content).unwrap().trim();

    assert!(version_str.contains("VERS_1.0"));
    assert!(version_str.contains(r#"extern "C++""#));
    assert!(version_str.contains("ns::*"));
}

#[test]
fn test_version_command_with_other_commands() {
    let script = parse_script(
        r"
            ENTRY(_start)
            VERSION {
                VERS_1.0 {
                    global: foo;
                };
            }
            SECTIONS {
                .text : { *(.text) }
            }
            ",
    )
    .unwrap();

    assert!(script.get_version_script_content().is_some());
    assert!(
        script
            .commands
            .iter()
            .any(|cmd| matches!(cmd, Command::Entry(_)))
    );
    assert!(
        script
            .commands
            .iter()
            .any(|cmd| matches!(cmd, Command::Sections(_)))
    );
}

#[test]
fn test_version_script_parsing_from_version_command() {
    use crate::script_data::ScriptData;
    use crate::version_script::VersionScript;

    let script = parse_script(
        r"
            VERSION {
                VERS_1.0 {
                    global: foo; bar*;
                    local: *;
                };
            }
            ",
    )
    .unwrap();

    let version_content = script.get_version_script_content().unwrap();

    let script_data = ScriptData {
        raw: version_content,
    };

    let version_script = VersionScript::parse(script_data).unwrap();

    assert_eq!(version_script.version_count(), 2);
}

#[test]
fn test_multiple_version_commands() {
    use crate::version_script::VersionScript;
    use crate::version_script::combine_version_script_bodies;

    let script = parse_script(
        r"
            VERSION {
                ver_a { global: foo; };
            }
            VERSION {
                ver_b { global: bar; } ver_a;
            }
            ",
    )
    .unwrap();

    let chunks: Vec<&[u8]> = script.version_script_contents().collect();
    assert_eq!(chunks.len(), 2);

    let intern = |bytes: &[u8]| -> &'static [u8] { Box::leak(bytes.to_vec().into_boxed_slice()) };
    let version_script = combine_version_script_bodies(&chunks, intern).unwrap();
    let VersionScript::Regular(regular) = version_script else {
        panic!("expected regular version script");
    };
    let names: Vec<&[u8]> = regular
        .version_iter()
        .filter(|version| !version.name.is_empty())
        .map(|version| version.name)
        .collect();
    assert_eq!(names, [b"ver_a".as_slice(), b"ver_b"]);
    assert_eq!(regular.versions[2].parent_index, Some(1));
}

#[test]
fn test_section_command_with_filename() {
    check_section_command(
        ".text : { foo.o(.text .text2) *(.text3) }",
        &SectionCommand::Section(Section {
            output_section_name: b".text",
            commands: vec![
                ContentsCommand::Matcher(Matcher {
                    must_keep: false,
                    input_file_pattern: Some(b"foo.o"),
                    exclude_file_patterns: vec![],
                    input_section_flags: InputSectionFlags::EMPTY,
                    input_section_name_patterns: vec![
                        SectionPattern {
                            name: b".text",
                            sort: SortKind::None,
                            reversed: false,
                        },
                        SectionPattern {
                            name: b".text2",
                            sort: SortKind::None,
                            reversed: false,
                        },
                    ],
                }),
                ContentsCommand::Matcher(Matcher {
                    must_keep: false,
                    input_file_pattern: None,
                    exclude_file_patterns: vec![],
                    input_section_flags: InputSectionFlags::EMPTY,
                    input_section_name_patterns: vec![SectionPattern {
                        name: b".text3",
                        sort: SortKind::None,
                        reversed: false,
                    }],
                }),
            ],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_section_command_with_glob_filename() {
    check_section_command(
        ".ctors : { *crtbegin*.o(.ctors) }",
        &SectionCommand::Section(Section {
            output_section_name: b".ctors",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: false,
                input_file_pattern: Some(b"*crtbegin*.o"),
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".ctors",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_keep_with_filename() {
    check_section_command(
        ".init : { KEEP(crti.o(.init)) }",
        &SectionCommand::Section(Section {
            output_section_name: b".init",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: true,
                input_file_pattern: Some(b"crti.o"),
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".init",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_assert_command() {
    check_linker_script(
        r#"
            SECTIONS {
                .text : { *(.text) }
            }
            ASSERT(. < 0x10000, "Output too large");
            "#,
        &LinkerScript {
            commands: vec![
                Command::Sections(Sections {
                    commands: vec![SectionCommand::Section(Section {
                        output_section_name: b".text",
                        commands: vec![ContentsCommand::Matcher(Matcher {
                            must_keep: false,
                            input_file_pattern: None,
                            exclude_file_patterns: vec![],
                            input_section_flags: InputSectionFlags::EMPTY,
                            input_section_name_patterns: vec![SectionPattern {
                                name: b".text",
                                sort: SortKind::None,
                                reversed: false,
                            }],
                        })],
                        alignment: None,
                        align_with_input: false,
                        subalign: None,
                        start_address_expression: None,
                        phdrs: vec![],
                        at_address: None,
                        region: None,
                        at_region: None,
                        fill: None,
                        attributes: None,
                        only_if: None,
                    })],
                }),
                Command::Assert(AssertCommand {
                    expression: Box::new(Expression::LessThan(
                        Box::new(Expression::LocationCounter),
                        Box::new(Expression::Number(0x10000)),
                    )),
                    message: b"Output too large",
                    remainder: b"",
                }),
            ],
        },
    );
}

#[test]
fn test_assert_in_sections() {
    check_linker_script(
        r#"
            SECTIONS {
                .text : { *(.text) }
                ASSERT(SIZEOF(.text) < 0x1000, "Text section too large")
            }
            "#,
        &LinkerScript {
            commands: vec![Command::Sections(Sections {
                commands: vec![
                    SectionCommand::Section(Section {
                        output_section_name: b".text",
                        commands: vec![ContentsCommand::Matcher(Matcher {
                            must_keep: false,
                            input_file_pattern: None,
                            exclude_file_patterns: vec![],
                            input_section_flags: InputSectionFlags::EMPTY,
                            input_section_name_patterns: vec![SectionPattern {
                                name: b".text",
                                sort: SortKind::None,
                                reversed: false,
                            }],
                        })],
                        alignment: None,
                        align_with_input: false,
                        subalign: None,
                        start_address_expression: None,
                        phdrs: vec![],
                        at_address: None,
                        region: None,
                        at_region: None,
                        fill: None,
                        attributes: None,
                        only_if: None,
                    }),
                    SectionCommand::Assert(AssertCommand {
                        expression: Box::new(Expression::LessThan(
                            Box::new(Expression::Sizeof(b".text")),
                            Box::new(Expression::Number(0x1000)),
                        )),
                        message: b"Text section too large",
                        remainder: b"",
                    }),
                ],
            })],
        },
    );
}

#[test]
fn test_assert_with_complex_expression() {
    let script =
        parse_script(r#"ASSERT(__bss_end - __bss_start <= 0x1000, "BSS too large");"#).unwrap();

    assert_eq!(script.commands.len(), 1);
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::LessEqual(
                    Box::new(Expression::Subtract(
                        Box::new(Expression::Symbol(b"__bss_end")),
                        Box::new(Expression::Symbol(b"__bss_start")),
                    )),
                    Box::new(Expression::Number(0x1000)),
                )
            );
            assert_eq!(assert_cmd.message, b"BSS too large");
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_expression_operator_precedence() {
    // Test that multiplication has higher precedence than addition: 1 + 2 * 3 = 7
    let script = parse_script(r#"ASSERT(1 + 2 * 3 == 7, "Math is broken");"#).unwrap();

    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::Add(
                        Box::new(Expression::Number(1)),
                        Box::new(Expression::Multiply(
                            Box::new(Expression::Number(2)),
                            Box::new(Expression::Number(3)),
                        )),
                    )),
                    Box::new(Expression::Number(7)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_assert_with_min_function_comma_handling() {
    // This is the KEY test for comma handling!
    // MIN(a, b) has a comma INSIDE the function call
    // The old code would have stopped at the first comma and failed
    let script = parse_script(
        r#"ASSERT(MIN(SIZEOF(.text), SIZEOF(.data)) < 0x10000, "Section too large");"#,
    )
    .unwrap();

    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            // Verify it parsed as LessThan with MIN function
            assert_matches!(*assert_cmd.expression, Expression::LessThan(_, _));
            if let Expression::LessThan(left, _) = &*assert_cmd.expression {
                // The left side should be a MIN expression with two SIZEOF calls
                assert_matches!(**left, Expression::Min(_, _));
            }
            assert_eq!(assert_cmd.message, b"Section too large");
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_bitwise_operators() {
    // & should bind tighter than ==, so `0xFF & 0x0F == 0x0F` parses as `(0xFF & 0x0F) == 0x0F`
    let script = parse_script(r#"ASSERT(0xFF & 0x0F == 0x0F, "mask test");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::BitwiseAnd(
                        Box::new(Expression::Number(0xFF)),
                        Box::new(Expression::Number(0x0F)),
                    )),
                    Box::new(Expression::Number(0x0F)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }

    // Test that | and ^ parse correctly: `1 | 2 ^ 3` should be `1 | (2 ^ 3)` since ^ binds
    // tighter
    let script = parse_script(r#"ASSERT(1 | 2 ^ 3 == 1, "bitwise test");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            // The == binds loosest, so the top level is Equal
            assert_matches!(*assert_cmd.expression, Expression::Equal(_, _));
            if let Expression::Equal(left, _) = &*assert_cmd.expression {
                // Left side should be BitwiseOr(1, BitwiseXor(2, 3))
                assert_matches!(**left, Expression::BitwiseOr(_, _));
                if let Expression::BitwiseOr(or_left, or_right) = &**left {
                    assert_eq!(**or_left, Expression::Number(1));
                    assert_matches!(**or_right, Expression::BitwiseXor(_, _));
                }
            }
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_shift_operators() {
    // 1 << 3 should parse as LeftShift(1, 3)
    let script = parse_script(r#"ASSERT(1 << 3 == 8, "shift test");"#).unwrap();

    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::LeftShift(
                        Box::new(Expression::Number(1)),
                        Box::new(Expression::Number(3)),
                    )),
                    Box::new(Expression::Number(8)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_logical_operators() {
    // 1 && 0 || 1 should parse as LogicalOr(LogicalAnd(1, 0), 1)
    // because && binds tighter than ||
    let script = parse_script(r#"ASSERT(1 && 0 || 1, "logical test");"#).unwrap();

    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::LogicalOr(
                    Box::new(Expression::LogicalAnd(
                        Box::new(Expression::Number(1)),
                        Box::new(Expression::Number(0)),
                    )),
                    Box::new(Expression::Number(1)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_unary_operators() {
    // !0 should parse as LogicalNot(0)
    let script = parse_script(r#"ASSERT(!0, "not zero");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::LogicalNot(Box::new(Expression::Number(0)))
            );
        }
        _ => panic!("Expected Assert command"),
    }

    // ~0xFF should parse as BitwiseNot(0xFF)
    let script = parse_script(r#"ASSERT(~0xFF == 0, "bitwise not");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::BitwiseNot(Box::new(Expression::Number(0xFF)))),
                    Box::new(Expression::Number(0)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }

    // -1 should parse as Negate(1)
    let script = parse_script(r#"ASSERT(-1 == 0, "negate");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::Negate(Box::new(Expression::Number(1)))),
                    Box::new(Expression::Number(0)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_unary_precedence() {
    // ~0xFF & 0xFF should parse as (BitwiseNot(0xFF)) & 0xFF
    // because unary binds tighter than binary
    let script = parse_script(r#"ASSERT(~0xFF & 0xFF == 0, "unary precedence");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            if let Expression::Equal(left, _) = &*assert_cmd.expression {
                assert_eq!(
                    **left,
                    Expression::BitwiseAnd(
                        Box::new(Expression::BitwiseNot(Box::new(Expression::Number(0xFF)))),
                        Box::new(Expression::Number(0xFF)),
                    )
                );
            } else {
                panic!("Expected Equal at top level");
            }
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_alignof_parsing() {
    let script = parse_script(r#"ASSERT(ALIGNOF(.text) == 8, "align test");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::Alignof(b".text")),
                    Box::new(Expression::Number(8)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_alignof_next_section_parsing() {
    let script = parse_script(r#"ASSERT(ALIGNOF(NEXT_SECTION) == 32, "next");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::Alignof(b"NEXT_SECTION")),
                    Box::new(Expression::Number(32)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_sizeof_next_section_parsing() {
    let script = parse_script(r#"ASSERT(SIZEOF(NEXT_SECTION) == 16, "next");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::Sizeof(b"NEXT_SECTION")),
                    Box::new(Expression::Number(16)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_rewrite_next_section() {
    let expr = Expression::Align(Box::new(Expression::Alignof(b"NEXT_SECTION")), None);
    assert!(expr.contains_next_section());
    assert_eq!(
        expr.rewrite_next_section(32, 8),
        Expression::Align(Box::new(Expression::Number(32)), None)
    );
    let size_expr = Expression::Sizeof(b"NEXT_SECTION");
    assert_eq!(size_expr.rewrite_next_section(32, 8), Expression::Number(8));
}

#[test]
fn test_loadaddr_parsing() {
    let script = parse_script(r#"ASSERT(LOADADDR(.text) == 8, "loadaddr test");"#).unwrap();
    match &script.commands[0] {
        Command::Assert(assert_cmd) => {
            assert_eq!(
                *assert_cmd.expression,
                Expression::Equal(
                    Box::new(Expression::Loadaddr(b".text")),
                    Box::new(Expression::Number(8)),
                )
            );
        }
        _ => panic!("Expected Assert command"),
    }
}

#[test]
fn test_number_suffixes() {
    let cases = [("1K", 1024), ("2k", 2048), ("1M", 1048576), ("2m", 2097152)];

    for (input, expected) in cases {
        let mut bstr = winnow::BStr::new(input.as_bytes());
        let expr = parse_expression.parse_next(&mut bstr).unwrap();
        assert_eq!(expr, Expression::Number(expected));
    }
}

#[test]
fn test_memory_block_parsing() {
    let script = parse_script(
        r"MEMORY {
                rom : ORIGIN = 256K, LENGTH = 1M
                ram : org = 0x20000000, l = 32K
            }",
    )
    .unwrap();
    let Command::Memory(regions) = &script.commands[0] else {
        panic!("Expected Memory command")
    };
    assert_eq!(regions.len(), 2);
    assert_eq!(regions[0].name, b"rom");
    assert_eq!(regions[0].origin, Expression::Number(262144));
    assert_eq!(regions[0].length, Expression::Number(1048576));
    assert_eq!(regions[1].name, b"ram");
    assert_eq!(regions[1].origin, Expression::Number(0x20000000));
    assert_eq!(regions[1].length, Expression::Number(32768));
}

#[test]
fn test_memory_functions_parsing() {
    let cases = [
        (
            r#"ASSERT(ORIGIN(rom) == 256K, "");"#,
            Expression::Origin(b"rom"),
            262144u64,
        ),
        (
            r#"ASSERT(LENGTH(ram) == 32K, "");"#,
            Expression::Length(b"ram"),
            32768,
        ),
    ];
    for (input, expected_fn, expected_val) in cases {
        let script = parse_script(input).unwrap();
        let Command::Assert(cmd) = &script.commands[0] else {
            panic!()
        };
        assert_eq!(
            *cmd.expression,
            Expression::Equal(
                Box::new(expected_fn),
                Box::new(Expression::Number(expected_val))
            )
        );
    }
}

#[test]
fn test_output_format_parsing() {
    let unquoted = parse_script(
        r"OUTPUT_FORMAT(elf64-x86-64)
            OUTPUT_FORMAT(elf64-x86-64, elf64-x86-64, elf64-x86-64)
            ",
    )
    .unwrap();

    let quoted = parse_script(
        r#"OUTPUT_FORMAT("elf64-x86-64")
            OUTPUT_FORMAT("elf64-x86-64", "elf64-x86-64", "elf64-x86-64")
            "#,
    )
    .unwrap();

    assert_eq!(unquoted, quoted);
}

#[test]
fn test_output_arch_parsing() {
    let script = parse_script(
        r#"OUTPUT_ARCH(i386:x86-64)
            OUTPUT_ARCH("aarch64")
            "#,
    )
    .unwrap();
    assert_eq!(
        script.commands,
        vec![
            Command::OutputArch(b"i386:x86-64"),
            Command::OutputArch(b"aarch64"),
        ]
    );
}

#[test]
fn test_insert_after() {
    let script = parse_script(
        r#"
        SECTIONS { .foo : { *(.foo) } }
        INSERT AFTER .text;
        "#,
    )
    .unwrap();
    assert!(matches!(
        script.commands.last(),
        Some(Command::Insert {
            after: true,
            section_name: b".text"
        })
    ));
}

#[test]
fn test_insert_before() {
    let script = parse_script("INSERT BEFORE .data;").unwrap();
    assert_eq!(
        script.commands,
        vec![Command::Insert {
            after: false,
            section_name: b".data"
        }]
    );
}

#[test]
fn test_output_search_dir_startup_target() {
    let script = parse_script(
        r#"
        OUTPUT("a.out")
        SEARCH_DIR(/usr/lib)
        STARTUP(crt0.o)
        TARGET(elf64-x86-64)
        "#,
    )
    .unwrap();
    assert_eq!(
        script.commands,
        vec![
            Command::Output(b"a.out"),
            Command::SearchDir(b"/usr/lib"),
            Command::Startup(b"crt0.o"),
            Command::Target(b"elf64-x86-64"),
        ]
    );
    assert_eq!(script.output_filename(), Some(&b"a.out"[..]));
    assert_eq!(script.search_dirs(), vec![&b"/usr/lib"[..]]);
}

#[test]
fn test_force_common_allocation() {
    let script = parse_script(
        r#"
        FORCE_COMMON_ALLOCATION
        FORCE_COMMON_ALLOCATION;
        "#,
    )
    .unwrap();
    assert_eq!(
        script.commands,
        vec![
            Command::ForceCommonAllocation,
            Command::ForceCommonAllocation,
        ]
    );
    assert!(script.force_common_allocation());
    assert!(
        !parse_script("SEARCH_DIR(/usr/lib)")
            .unwrap()
            .force_common_allocation()
    );
}

#[test]
fn test_include_sees_preceding_search_dir() {
    let mut script = parse_script(
        r#"
        SEARCH_DIR("inc")
        INCLUDE(fragment.ld)
        "#,
    )
    .unwrap();
    let mut seen_dirs: Option<Vec<Vec<u8>>> = None;
    script
        .expand_includes(&mut |path, dirs| {
            assert_eq!(path, b"fragment.ld");
            seen_dirs = Some(dirs.iter().map(|d| d.to_vec()).collect());
            Ok(b"from_include = 1;".as_slice())
        })
        .unwrap();
    assert_eq!(seen_dirs.unwrap(), vec![b"inc".to_vec()]);
}

#[test]
fn test_region_alias_and_nocrossrefs() {
    let script = parse_script(
        r#"
        REGION_ALIAS("ALIAS_RAM", RAM)
        NOCROSSREFS(.text .data)
        NOCROSSREFS_TO(.text .init .fini)
        "#,
    )
    .unwrap();
    assert_eq!(
        script.commands,
        vec![
            Command::RegionAlias {
                alias: b"ALIAS_RAM",
                region: b"RAM"
            },
            Command::Nocrossrefs(vec![b".text", b".data"]),
            Command::NocrossrefsTo {
                to: b".text",
                from: vec![b".init", b".fini"]
            },
        ]
    );
}

#[test]
fn test_extern_and_hidden() {
    let script = parse_script(
        r#"
        EXTERN(_zimage_start foo)
        HIDDEN(hidden_abs = 0x1234);
        visible_abs = 0x5678;
        SECTIONS {
            .text : {
                HIDDEN(_hidden_start = .);
                *(.text)
                visible_end = .;
            }
        }
        "#,
    )
    .unwrap();
    assert_eq!(
        &script.commands[..3],
        &[
            Command::Extern(vec![b"_zimage_start", b"foo"]),
            Command::SymbolDefinition {
                name: b"hidden_abs",
                value: Expression::Number(0x1234),
                hidden: true,
            },
            Command::SymbolDefinition {
                name: b"visible_abs",
                value: Expression::Number(0x5678),
                hidden: false,
            },
        ]
    );
    let Command::Sections(sections) = &script.commands[3] else {
        panic!("expected SECTIONS");
    };
    let SectionCommand::Section(sec) = &sections.commands[0] else {
        panic!("expected output section");
    };
    assert!(matches!(
        &sec.commands[0],
        ContentsCommand::SymbolAssignment(SymbolAssignment {
            name: b"_hidden_start",
            hidden: true,
            ..
        })
    ));
    assert!(matches!(
        &sec.commands[2],
        ContentsCommand::SymbolAssignment(SymbolAssignment {
            name: b"visible_end",
            hidden: false,
            ..
        })
    ));
}

#[test]
fn test_startup_is_first_foreach_input() {
    let inputs = inputs_from_script("INPUT(second.o) STARTUP(first.o) INPUT(third.o)").unwrap();
    assert_eq!(
        inputs
            .iter()
            .map(|i| (i.startup, i.spec.clone()))
            .collect::<Vec<_>>(),
        vec![
            (true, InputSpec::File(Box::from(Path::new("first.o")))),
            (false, InputSpec::File(Box::from(Path::new("second.o")))),
            (false, InputSpec::File(Box::from(Path::new("third.o")))),
        ]
    );
}

#[test]
fn test_nested_sort_name_and_alignment() {
    check_section_command(
        ".text : { *(SORT_BY_NAME(SORT_BY_ALIGNMENT(.text.*))) }",
        &reverse_section(SectionPattern {
            name: b".text.*",
            sort: SortKind::NameThenAlignment,
            reversed: false,
        }),
    );
    check_section_command(
        ".text : { *(SORT(SORT_BY_ALIGNMENT(.text.*))) }",
        &reverse_section(SectionPattern {
            name: b".text.*",
            sort: SortKind::NameThenAlignment,
            reversed: false,
        }),
    );
    check_section_command(
        ".text : { *(SORT_BY_ALIGNMENT(SORT_BY_NAME(.text.*))) }",
        &reverse_section(SectionPattern {
            name: b".text.*",
            sort: SortKind::AlignmentThenName,
            reversed: false,
        }),
    );
    check_section_command(
        ".text : { *(SORT_BY_NAME(SORT_BY_NAME(.text.*))) }",
        &reverse_section(SectionPattern {
            name: b".text.*",
            sort: SortKind::Name,
            reversed: false,
        }),
    );
    check_section_command(
        ".text : { *(SORT_BY_ALIGNMENT(SORT_BY_ALIGNMENT(.text.*))) }",
        &reverse_section(SectionPattern {
            name: b".text.*",
            sort: SortKind::Alignment,
            reversed: false,
        }),
    );
}

#[test]
fn test_nested_sort_is_unsupported() {
    let script = parse_script(
        r"
            SECTIONS {
                .text : {
                    *(SORT_BY_INIT_PRIORITY(SORT_BY_NAME(.text.*)))
                }
            }
            ",
    );
    assert!(script.is_err());
}

fn reverse_section(pattern: SectionPattern<'_>) -> SectionCommand<'_> {
    SectionCommand::Section(Section {
        output_section_name: b".text",
        commands: vec![ContentsCommand::Matcher(Matcher {
            must_keep: false,
            input_file_pattern: None,
            exclude_file_patterns: vec![],
            input_section_flags: InputSectionFlags::EMPTY,
            input_section_name_patterns: vec![pattern],
        })],
        alignment: None,
        align_with_input: false,
        subalign: None,
        start_address_expression: None,
        phdrs: vec![],
        at_address: None,
        region: None,
        at_region: None,
        fill: None,
        attributes: None,
        only_if: None,
    })
}

#[test]
fn test_reverse_parsing() {
    check_section_command(
        ".text : { *(REVERSE(.text.rev.*)) }",
        &reverse_section(SectionPattern {
            name: b".text.rev.*",
            sort: SortKind::Name,
            reversed: true,
        }),
    );
    check_section_command(
        ".text : { *(REVERSE(SORT(.text.rev.*))) }",
        &reverse_section(SectionPattern {
            name: b".text.rev.*",
            sort: SortKind::Name,
            reversed: true,
        }),
    );
    check_section_command(
        ".text : { *(SORT_BY_NAME(REVERSE(.text.rev.*))) }",
        &reverse_section(SectionPattern {
            name: b".text.rev.*",
            sort: SortKind::Name,
            reversed: true,
        }),
    );
    check_section_command(
        ".text : { *(SORT_BY_INIT_PRIORITY(REVERSE(.init_array.*))) }",
        &reverse_section(SectionPattern {
            name: b".init_array.*",
            sort: SortKind::InitPriority,
            reversed: true,
        }),
    );
}

#[test]
fn test_reverse_alignment_is_unsupported() {
    assert!(
        parse_script("SECTIONS { .text : { *(REVERSE(SORT_BY_ALIGNMENT(.text.*))) } }").is_err()
    );
    assert!(
        parse_script("SECTIONS { .text : { *(SORT_BY_ALIGNMENT(REVERSE(.text.*))) } }").is_err()
    );
}

#[test]
fn test_reverse_wrapping_init_priority_is_unsupported() {
    assert!(
        parse_script("SECTIONS { .text : { *(REVERSE(SORT_BY_INIT_PRIORITY(.init_array.*))) } }")
            .is_err()
    );
}

fn flags_section(flags: InputSectionFlags) -> SectionCommand<'static> {
    SectionCommand::Section(Section {
        output_section_name: b".text",
        commands: vec![ContentsCommand::Matcher(Matcher {
            must_keep: false,
            input_file_pattern: None,
            exclude_file_patterns: vec![],
            input_section_flags: flags,
            input_section_name_patterns: vec![SectionPattern {
                name: b".text",
                sort: SortKind::None,
                reversed: false,
            }],
        })],
        alignment: None,
        align_with_input: false,
        subalign: None,
        start_address_expression: None,
        phdrs: vec![],
        at_address: None,
        region: None,
        at_region: None,
        fill: None,
        attributes: None,
        only_if: None,
    })
}

#[test]
fn test_input_section_flags_parsing() {
    check_section_command(
        ".text : { INPUT_SECTION_FLAGS(SHF_ALLOC) *(.text) }",
        &flags_section(InputSectionFlags {
            with: object::elf::SHF_ALLOC.0,
            without: 0,
        }),
    );
    check_section_command(
        ".text : { INPUT_SECTION_FLAGS(SHF_MERGE & SHF_STRINGS) *(.text) }",
        &flags_section(InputSectionFlags {
            with: object::elf::SHF_MERGE.0 | object::elf::SHF_STRINGS.0,
            without: 0,
        }),
    );
    check_section_command(
        ".text : { INPUT_SECTION_FLAGS(!SHF_WRITE) *(.text) }",
        &flags_section(InputSectionFlags {
            with: 0,
            without: object::elf::SHF_WRITE.0,
        }),
    );
    check_section_command(
        ".text : { INPUT_SECTION_FLAGS(SHF_ALLOC & !SHF_WRITE) *(.text) }",
        &flags_section(InputSectionFlags {
            with: object::elf::SHF_ALLOC.0,
            without: object::elf::SHF_WRITE.0,
        }),
    );
    check_section_command(
        ".text : { INPUT_SECTION_FLAGS(0x2) *(.text) }",
        &flags_section(InputSectionFlags {
            with: object::elf::SHF_ALLOC.0,
            without: 0,
        }),
    );
    check_section_command(
        ".text : { KEEP(INPUT_SECTION_FLAGS(!SHF_WRITE) *(.text)) }",
        &SectionCommand::Section(Section {
            output_section_name: b".text",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: true,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags {
                    with: 0,
                    without: object::elf::SHF_WRITE.0,
                },
                input_section_name_patterns: vec![SectionPattern {
                    name: b".text",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_unrecognised_input_section_flag() {
    assert!(
        parse_script("SECTIONS { .text : { INPUT_SECTION_FLAGS(SHF_NOT_A_FLAG) *(.text) } }")
            .is_err()
    );
}

#[test]
fn test_align_with_input_parsing() {
    check_section_command(
        ".data : ALIGN_WITH_INPUT { *(.data) }",
        &SectionCommand::Section(Section {
            output_section_name: b".data",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: false,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".data",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: true,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_align_with_input_rejects_explicit_align() {
    assert!(parse_script("SECTIONS { .data : ALIGN(16) ALIGN_WITH_INPUT { *(.data) } }").is_err());
    assert!(parse_script("SECTIONS { .data : ALIGN_WITH_INPUT ALIGN(16) { *(.data) } }").is_err());
}

#[test]
fn test_absolute_parsing() {
    let mut bstr = winnow::BStr::new(b"ABSOLUTE(startup_64 - 0x400000)");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(
        expr,
        Expression::Absolute(Box::new(Expression::Subtract(
            Box::new(Expression::Symbol(b"startup_64")),
            Box::new(Expression::Number(0x400000)),
        )))
    );
}

#[test]
fn test_log2ceil_and_block_parsing() {
    let mut bstr = winnow::BStr::new(b"LOG2CEIL(3)");
    assert_eq!(
        parse_expression.parse_next(&mut bstr).unwrap(),
        Expression::Log2Ceil(Box::new(Expression::Number(3)))
    );
    assert_eq!(
        parse_expression
            .parse_next(&mut winnow::BStr::new(b"LOG2CEIL(3)"))
            .unwrap()
            .relocatable_anchor(),
        None
    );

    let mut bstr = winnow::BStr::new(b"BLOCK(16)");
    assert_eq!(
        parse_expression.parse_next(&mut bstr).unwrap(),
        Expression::Align(Box::new(Expression::Number(16)), None)
    );
}

#[test]
fn test_relocatable_anchor() {
    let mut bstr = winnow::BStr::new(b"jiffies_64");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(
        expr.relocatable_anchor(),
        Some(RelocatableAnchor::Symbol(b"jiffies_64"))
    );

    let mut bstr = winnow::BStr::new(b"jiffies_64 + 4");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(expr.relocatable_anchor(), None);

    let mut bstr = winnow::BStr::new(b"4 + jiffies_64");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(expr.relocatable_anchor(), None);

    let mut bstr = winnow::BStr::new(b"_etext + 4");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(expr.relocatable_anchor(), None);

    let mut bstr = winnow::BStr::new(b"_etext - _stext");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(expr.relocatable_anchor(), None);

    let mut bstr = winnow::BStr::new(b"ABSOLUTE(startup_64 - 0x400000)");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(expr.relocatable_anchor(), None);

    let mut bstr = winnow::BStr::new(b".");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(
        expr.relocatable_anchor(),
        Some(RelocatableAnchor::LocationCounter)
    );

    let mut bstr = winnow::BStr::new(b"ALIGN(8)");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(
        expr.relocatable_anchor(),
        Some(RelocatableAnchor::LocationCounter)
    );

    let mut bstr = winnow::BStr::new(b". + 8");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(
        expr.relocatable_anchor(),
        Some(RelocatableAnchor::LocationCounter)
    );

    let mut bstr = winnow::BStr::new(b"0xabcd");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_eq!(expr.relocatable_anchor(), None);
}

#[test]
fn test_data_segment_and_constant() {
    let mut bstr = winnow::BStr::new(b"CONSTANT(MAXPAGESIZE)");
    assert_eq!(
        parse_expression.parse_next(&mut bstr).unwrap(),
        Expression::ConstantMaxPageSize
    );

    let mut bstr = winnow::BStr::new(b"CONSTANT(COMMONPAGESIZE)");
    assert_eq!(
        parse_expression.parse_next(&mut bstr).unwrap(),
        Expression::ConstantCommonPageSize
    );

    let mut bstr =
        winnow::BStr::new(b"DATA_SEGMENT_ALIGN(CONSTANT(MAXPAGESIZE), CONSTANT(COMMONPAGESIZE))");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_matches!(expr, Expression::DataSegmentAlign(_, _));
    assert_eq!(
        expr.relocatable_anchor(),
        Some(RelocatableAnchor::LocationCounter)
    );

    let mut bstr = winnow::BStr::new(b"DATA_SEGMENT_RELRO_END(0, .)");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_matches!(expr, Expression::DataSegmentRelroEnd(_, _));

    let mut bstr = winnow::BStr::new(b"DATA_SEGMENT_END(.)");
    let expr = parse_expression.parse_next(&mut bstr).unwrap();
    assert_matches!(expr, Expression::DataSegmentEnd(_));
}

#[test]
fn test_only_if_and_sort_none() {
    check_section_command(
        ".eh_frame : ONLY_IF_RO { KEEP (*(.eh_frame)) }",
        &SectionCommand::Section(Section {
            output_section_name: b".eh_frame",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: true,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".eh_frame",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: Some(OnlyIf::Ro),
        }),
    );
    check_section_command(
        ".init : { KEEP (*(SORT_NONE(.init))) }",
        &SectionCommand::Section(Section {
            output_section_name: b".init",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: true,
                input_file_pattern: None,
                exclude_file_patterns: vec![],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![SectionPattern {
                    name: b".init",
                    sort: SortKind::None,
                    reversed: false,
                }],
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_exclude_file_between_patterns() {
    check_section_command(
        ".init_array : { KEEP (*(.init_array EXCLUDE_FILE (*crtbegin.o *crtend.o) .ctors)) }",
        &SectionCommand::Section(Section {
            output_section_name: b".init_array",
            commands: vec![ContentsCommand::Matcher(Matcher {
                must_keep: true,
                input_file_pattern: None,
                exclude_file_patterns: vec![b"*crtbegin.o", b"*crtend.o"],
                input_section_flags: InputSectionFlags::EMPTY,
                input_section_name_patterns: vec![
                    SectionPattern {
                        name: b".init_array",
                        sort: SortKind::None,
                        reversed: false,
                    },
                    SectionPattern {
                        name: b".ctors",
                        sort: SortKind::None,
                        reversed: false,
                    },
                ],
            })],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: None,
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: None,
            only_if: None,
        }),
    );
}

#[test]
fn test_linker_version_in_comment() {
    check_section_command(
        ".comment 0 (INFO) : { *(.comment); LINKER_VERSION; }",
        &SectionCommand::Section(Section {
            output_section_name: b".comment",
            commands: vec![
                ContentsCommand::Matcher(Matcher {
                    must_keep: false,
                    input_file_pattern: None,
                    exclude_file_patterns: vec![],
                    input_section_flags: InputSectionFlags::EMPTY,
                    input_section_name_patterns: vec![SectionPattern {
                        name: b".comment",
                        sort: SortKind::None,
                        reversed: false,
                    }],
                }),
                ContentsCommand::LinkerVersion,
            ],
            alignment: None,
            align_with_input: false,
            subalign: None,
            start_address_expression: Some(Expression::Number(0)),
            phdrs: vec![],
            at_address: None,
            region: None,
            at_region: None,
            fill: None,
            attributes: Some(SectionAttributes::Info),
            only_if: None,
        }),
    );
}

#[test]
fn test_gnu_default_shared_script_parses() {
    let script = include_str!("test_data/gnu-default-shared.ld");
    parse_script(script).unwrap();
}
