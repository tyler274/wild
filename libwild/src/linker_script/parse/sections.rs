use super::*;
use crate::alignment::Alignment;
use crate::linker_script::ast::*;
use winnow::BStr;
use winnow::Parser as _;
use winnow::ascii::dec_uint;
use winnow::combinator::alt;
use winnow::combinator::eof;
use winnow::combinator::opt;
use winnow::combinator::repeat_till;
use winnow::error::ContextError;
use winnow::error::FromExternalError;
use winnow::token::take_while;

pub(crate) fn parse_section_command_list<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<Vec<SectionCommand<'input>>> {
    skip_comments_and_whitespace(input)?;
    Ok(repeat_till(0.., parse_section_command, eof)
        .parse_next(input)?
        .0)
}
pub(crate) fn parse_sections<'input>(input: &mut &'input BStr) -> winnow::Result<Sections<'input>> {
    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let (commands, _) = repeat_till(0.., parse_section_command, '}').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(Sections { commands })
}

pub(crate) fn parse_section_command<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<SectionCommand<'input>> {
    let name = parse_token(input)?;

    skip_comments_and_whitespace(input)?;

    // Handle ASSERT command
    match name {
        b"ASSERT" => {
            return Ok(SectionCommand::Assert(parse_assert(input)?));
        }
        b"PROVIDE" => {
            return Ok(SectionCommand::Provide(parse_provide(input, false)?));
        }
        b"PROVIDE_HIDDEN" => {
            return Ok(SectionCommand::Provide(parse_provide(input, true)?));
        }
        b"OVERLAY" => {
            return parse_overlay(input, None);
        }
        b"INCLUDE" => {
            let path = parse_include_path(input)?;
            return Ok(SectionCommand::Include(path));
        }
        _ => {}
    }

    if let Some(op) = opt(parse_assignment_op).parse_next(input)? {
        skip_comments_and_whitespace(input)?;
        let expr = parse_expression.parse_next(input)?;
        skip_comments_and_whitespace(input)?;
        ';'.parse_next(input)?;
        skip_comments_and_whitespace(input)?;
        let expanded = op.expand(name, expr);
        if name == b"." {
            return Ok(SectionCommand::SetLocation(Location { address: expanded }));
        }
        return Ok(SectionCommand::SymbolAssignment(SymbolAssignment {
            name,
            expr: expanded,
        }));
    }

    let mut start_address_expression = None;
    let mut section_type = None;
    while !input.starts_with(b":") && !input.starts_with(b"{") {
        if let Some(stype) = opt(parse_section_attribute).parse_next(input)? {
            section_type = Some(stype);
        } else {
            start_address_expression = Some(parse_expression.parse_next(input)?);
        }
        skip_comments_and_whitespace(input)?;
    }

    opt(':').parse_next(input)?;

    skip_comments_and_whitespace(input)?;

    let mut alignment = None;
    let mut at_address = None;
    let mut only_if = None;

    while !input.starts_with(b"{") {
        skip_comments_and_whitespace(input)?;
        if input.starts_with(b"AT>") {
            break;
        }
        if opt("ONLY_IF_RO").parse_next(input)?.is_some() {
            only_if = Some(OnlyIf::Ro);
        } else if opt("ONLY_IF_RW").parse_next(input)?.is_some() {
            only_if = Some(OnlyIf::Rw);
        } else if opt("AT").parse_next(input)?.is_some() {
            at_address = Some(parse_at_address.parse_next(input)?);
        } else {
            alignment = Some(parse_alignment.parse_next(input)?);
        }
        skip_comments_and_whitespace(input)?;
    }

    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let (commands, _) = repeat_till(0.., parse_contents_command, '}').parse_next(input)?;

    skip_comments_and_whitespace(input)?;

    let mut phdrs = vec![];
    let mut region = None;
    let mut at_region = None;
    let mut fill = None;
    loop {
        skip_comments_and_whitespace(input)?;
        if opt("AT>").parse_next(input)?.is_some() {
            skip_comments_and_whitespace(input)?;
            at_region = Some(parse_token(input)?);
            continue;
        }
        let Some(prefix) = opt(alt((b":", b">", b"="))).parse_next(input)? else {
            break;
        };
        skip_comments_and_whitespace(input)?;
        match prefix {
            b":" => phdrs.push(parse_token(input)?),
            b">" => region = Some(parse_token(input)?),
            b"=" => fill = Some(parse_fill(input)?),
            _ => unreachable!(),
        }
    }

    opt(',').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(SectionCommand::Section(Section {
        output_section_name: name,
        commands,
        alignment,
        start_address_expression,
        phdrs,
        at_address,
        region,
        at_region,
        fill,
        attributes: section_type,
        only_if,
    }))
}

pub(crate) fn parse_overlay<'input>(
    input: &mut &'input BStr,
    start_address: Option<Expression<'input>>,
) -> winnow::Result<SectionCommand<'input>> {
    skip_comments_and_whitespace(input)?;
    let mut start_address = start_address;
    if !input.starts_with(b":") && !input.starts_with(b"{") {
        start_address = Some(parse_expression.parse_next(input)?);
        skip_comments_and_whitespace(input)?;
    }
    ':'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let mut nocrossrefs = false;
    let mut at_address = None;
    while !input.starts_with(b"{") {
        if opt("NOCROSSREFS").parse_next(input)?.is_some() {
            nocrossrefs = true;
        } else if opt("AT").parse_next(input)?.is_some() {
            at_address = Some(parse_at_address.parse_next(input)?);
        } else {
            break;
        }
        skip_comments_and_whitespace(input)?;
    }

    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let mut sections = Vec::new();
    while !input.starts_with(b"}") {
        let SectionCommand::Section(section) = parse_section_command.parse_next(input)? else {
            return Err(ContextError::default());
        };
        sections.push(section);
        skip_comments_and_whitespace(input)?;
    }
    '}'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let mut phdrs = vec![];
    let mut region = None;
    let mut at_region = None;
    let mut fill = None;
    loop {
        skip_comments_and_whitespace(input)?;
        if opt("AT>").parse_next(input)?.is_some() {
            skip_comments_and_whitespace(input)?;
            at_region = Some(parse_token(input)?);
            continue;
        }
        let Some(prefix) = opt(alt((b":", b">", b"="))).parse_next(input)? else {
            break;
        };
        skip_comments_and_whitespace(input)?;
        match prefix {
            b":" => phdrs.push(parse_token(input)?),
            b">" => region = Some(parse_token(input)?),
            b"=" => fill = Some(parse_fill(input)?),
            _ => unreachable!(),
        }
    }

    opt(',').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(SectionCommand::Overlay(Overlay {
        start_address,
        at_address,
        nocrossrefs,
        sections,
        region,
        at_region,
        phdrs,
        fill,
    }))
}

pub(crate) fn parse_section_attribute(input: &mut &BStr) -> winnow::Result<SectionAttributes> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    if opt("READONLY").parse_next(input)?.is_some() {
        skip_comments_and_whitespace(input)?;
        if input.starts_with(b"(") {
            let ty = parse_nested_type_equals(input)?;
            skip_comments_and_whitespace(input)?;
            ')'.parse_next(input)?;
            return Ok(SectionAttributes::ReadonlyType(ty));
        }
        ')'.parse_next(input)?;
        return Ok(SectionAttributes::Readonly);
    }

    if opt("TYPE").parse_next(input)?.is_some() {
        skip_comments_and_whitespace(input)?;
        let ty = parse_type_equals_value(input)?;
        skip_comments_and_whitespace(input)?;
        ')'.parse_next(input)?;
        return Ok(SectionAttributes::Type(ty));
    }

    let section_type = parse_token.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let section_type = match section_type {
        b"NOLOAD" => SectionAttributes::Noload,
        b"DSECT" => SectionAttributes::Dsect,
        b"COPY" => SectionAttributes::Copy,
        b"INFO" => SectionAttributes::Info,
        b"OVERLAY" => SectionAttributes::Overlay,
        _ => {
            return Err(ContextError::default());
        }
    };
    ')'.parse_next(input)?;

    Ok(section_type)
}

fn parse_nested_type_equals(input: &mut &BStr) -> winnow::Result<u32> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    "TYPE".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let ty = parse_type_equals_value(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    Ok(ty)
}

fn parse_type_equals_value(input: &mut &BStr) -> winnow::Result<u32> {
    '='.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let expr = parse_expression.parse_next(input)?;
    resolve_output_section_type(&expr).map_err(|_| {
        ContextError::from_external_error(input, LinkerScriptError::InvalidSectionType)
    })
}

fn resolve_output_section_type(expr: &Expression<'_>) -> Result<u32, ()> {
    match expr {
        Expression::Symbol(b"SHT_PROGBITS") => Ok(object::elf::SHT_PROGBITS.0),
        Expression::Symbol(b"SHT_STRTAB") => Ok(object::elf::SHT_STRTAB.0),
        Expression::Symbol(b"SHT_NOTE") => Ok(object::elf::SHT_NOTE.0),
        Expression::Symbol(b"SHT_NOBITS") => Ok(object::elf::SHT_NOBITS.0),
        Expression::Symbol(b"SHT_INIT_ARRAY") => Ok(object::elf::SHT_INIT_ARRAY.0),
        Expression::Symbol(b"SHT_FINI_ARRAY") => Ok(object::elf::SHT_FINI_ARRAY.0),
        Expression::Symbol(b"SHT_PREINIT_ARRAY") => Ok(object::elf::SHT_PREINIT_ARRAY.0),
        Expression::Symbol(_) => Err(()),
        other => crate::expression_eval::evaluate_const(other)
            .map(|v| v as u32)
            .map_err(|_| ()),
    }
}

pub(crate) fn parse_fill<'input>(input: &mut &'input BStr) -> winnow::Result<Fill<'input>> {
    return Ok(Fill {
        value: parse_expression.parse_next(input)?,
    });
}

pub(crate) fn parse_alignment(input: &mut &BStr) -> winnow::Result<Alignment> {
    "ALIGN".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let raw_alignment = dec_uint.parse_next(input)?;
    let alignment = Alignment::new(raw_alignment).map_err(|_| {
        ContextError::from_external_error(input, LinkerScriptError::InvalidAlignment)
    })?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(alignment)
}

pub(crate) fn parse_at_address<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<Expression<'input>> {
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let address = parse_expression.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(address)
}

pub(crate) fn parse_contents_command<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    alt((
        parse_contents_provide,
        parse_contents_assert,
        parse_contents_fill,
        parse_output_data,
        parse_constructors,
        parse_matcher,
        parse_assignment,
    ))
    .parse_next(input)
}

pub(crate) fn parse_contents_fill<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    "FILL".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let value = parse_expression.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(ContentsCommand::Fill(Fill { value }))
}

pub(crate) fn parse_output_data<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    let width = alt((
        "QUAD".map(|_| OutputDataWidth::Quad),
        "LONG".map(|_| OutputDataWidth::Long),
        "SHORT".map(|_| OutputDataWidth::Short),
        "BYTE".map(|_| OutputDataWidth::Byte),
    ))
    .parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let value = parse_expression.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(ContentsCommand::OutputData(OutputData { width, value }))
}

pub(crate) fn parse_contents_provide<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    let hidden = alt(("PROVIDE_HIDDEN", "PROVIDE")).parse_next(input)? == b"PROVIDE_HIDDEN";
    skip_comments_and_whitespace(input)?;
    let provide = parse_provide(input, hidden)?;
    Ok(ContentsCommand::Provide(provide))
}

pub(crate) fn parse_assignment<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    let name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    let op = parse_assignment_op.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let expr = parse_expression.parse_next(input)?;

    let expanded = op.expand(name, expr);
    let cmd = if name == b"." {
        ContentsCommand::SetLocation(Location { address: expanded })
    } else {
        ContentsCommand::SymbolAssignment(SymbolAssignment {
            name,
            expr: expanded,
        })
    };

    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(cmd)
}

pub(crate) fn parse_constructors<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    if opt("SORT").parse_next(input)?.is_some() {
        skip_comments_and_whitespace(input)?;
        '('.parse_next(input)?;
        skip_comments_and_whitespace(input)?;
        "CONSTRUCTORS".parse_next(input)?;
        skip_comments_and_whitespace(input)?;
        ')'.parse_next(input)?;
    } else {
        alt(("CONSTRUCTORS", "LINKER_VERSION")).parse_next(input)?;
    }
    skip_comments_and_whitespace(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(ContentsCommand::Constructors)
}

pub(crate) fn parse_matcher<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    let matcher = alt((parse_keep, parse_matcher_pattern)).parse_next(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(ContentsCommand::Matcher(matcher))
}

pub(crate) fn parse_keep<'input>(input: &mut &'input BStr) -> winnow::Result<Matcher<'input>> {
    "KEEP".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    let mut matcher = parse_matcher_pattern(input)?;
    matcher.must_keep = true;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(matcher)
}

pub(crate) fn parse_exclude_file_list<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<Vec<&'input [u8]>> {
    "EXCLUDE_FILE".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let mut files = Vec::new();
    while !input.starts_with(b")") {
        files.push(parse_token(input)?);
        skip_comments_and_whitespace(input)?;
    }
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(files)
}

pub(crate) fn parse_matcher_pattern<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<Matcher<'input>> {
    let mut exclude_file_patterns = Vec::new();
    if input.starts_with(b"EXCLUDE_FILE") {
        exclude_file_patterns = parse_exclude_file_list(input)?;
    }

    // Parse the file pattern token (e.g., *, foo.o, *crtbegin*.o).
    let file_pattern = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let mut patterns = Vec::new();
    while !input.starts_with(b")") {
        if input.starts_with(b"EXCLUDE_FILE") {
            exclude_file_patterns.extend(parse_exclude_file_list(input)?);
        } else {
            patterns.push(parse_pattern(input)?);
        }
        skip_comments_and_whitespace(input)?;
    }
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // A bare `*` means "match all files", represented as None.
    let input_file_pattern = if file_pattern == b"*" {
        None
    } else {
        Some(file_pattern)
    };

    Ok(Matcher {
        must_keep: false,
        input_file_pattern,
        exclude_file_patterns,
        input_section_name_patterns: patterns,
    })
}

pub(crate) fn parse_sort(input: &mut &BStr) -> winnow::Result<SortKind> {
    alt((
        "SORT_BY_INIT_PRIORITY".map(|_| SortKind::InitPriority),
        "SORT_BY_NAME".map(|_| SortKind::Name),
        "SORT_BY_ALIGNMENT".map(|_| SortKind::Alignment),
        "SORT_NONE".map(|_| SortKind::None),
        "SORT".map(|_| SortKind::Name),
    ))
    .parse_next(input)
}

pub(crate) fn parse_pattern<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<SectionPattern<'input>> {
    let wrapped = opt(parse_sort).parse_next(input)?;
    let sort = wrapped.unwrap_or(SortKind::None);

    if wrapped.is_some() {
        skip_comments_and_whitespace(input)?;
        '('.parse_next(input)?;
        winnow::combinator::not(parse_sort)
            .parse_next(input)
            .map_err(|_: ContextError| {
                ContextError::from_external_error(input, LinkerScriptError::UnsupportedNestedSort)
            })?;
    }

    skip_comments_and_whitespace(input)?;
    let name = take_while(1.., |b: u8| b != b')' && !b.is_ascii_whitespace()).parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    if wrapped.is_some() {
        ')'.parse_next(input)?;
        skip_comments_and_whitespace(input)?;
    }

    Ok(SectionPattern { name, sort })
}

pub(crate) fn parse_contents_assert<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    "ASSERT".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let assert = parse_assert(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(ContentsCommand::Assert(assert))
}
