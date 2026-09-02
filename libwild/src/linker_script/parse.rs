use super::ast::*;
use crate::alignment::Alignment;
use crate::args::Input;
use crate::args::InputSpec;
use crate::args::Modifiers;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use object::Wrap;
use std::path::Path;
use winnow::BStr;
use winnow::Parser as _;
use winnow::ascii::dec_uint;
use winnow::ascii::hex_uint;
use winnow::ascii::multispace0;
use winnow::combinator::alt;
use winnow::combinator::delimited;
use winnow::combinator::eof;
use winnow::combinator::opt;
use winnow::combinator::preceded;
use winnow::combinator::repeat_till;
use winnow::error::ContextError;
use winnow::error::FromExternalError;
use winnow::token::one_of;
use winnow::token::take_until;
use winnow::token::take_while;

impl<'data> LinkerScript<'data> {
    pub(crate) fn parse(bytes: &'data [u8], path: &Path) -> Result<LinkerScript<'data>> {
        let commands = parse_commands.parse(BStr::new(bytes)).map_err(|error| {
            error!(
                "Failed to parse linker script `{}`:\n{error}",
                path.display()
            )
        })?;

        Ok(LinkerScript { commands })
    }

    /// Recursively expand `INCLUDE` commands. `load` maps an include path to the included file's
    /// bytes (which must live as long as `'data`).
    pub(crate) fn expand_includes(
        &mut self,
        load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    ) -> Result {
        let mut stack = Vec::new();
        self.commands = expand_commands(std::mem::take(&mut self.commands), load, &mut stack)?;
        Ok(())
    }

    pub(crate) fn foreach_input(
        &self,
        starting_modifiers: Modifiers,
        mut cb: impl FnMut(Input) -> Result,
    ) -> Result {
        foreach_input(&self.commands, starting_modifiers, &mut cb)?;
        Ok(())
    }

    pub(crate) fn get_version_script_content(&self) -> Option<&'data [u8]> {
        self.commands.iter().find_map(|cmd| match cmd {
            Command::Version(content) => Some(*content),
            _ => None,
        })
    }
}

fn parse_token<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    if input.starts_with(b"\"") {
        '"'.parse_next(input)?;
        let content = take_until(0.., "\"").parse_next(input)?;
        '"'.parse_next(input)?;

        Ok(content)
    } else {
        take_while(1.., |b| !b" (){};,\n\t".contains(&b)).parse_next(input)
    }
}

pub(crate) fn skip_comments_and_whitespace(input: &mut &BStr) -> winnow::Result<()> {
    loop {
        multispace0(input)?;

        if input.starts_with(b"#") {
            take_until(1.., "\n").parse_next(input)?;
        } else if input.starts_with(b"/*") {
            take_until(1.., "*/")
                .parse_next(input)
                .map_err(|_: ContextError| {
                    ContextError::from_external_error(input, LinkerScriptError::UnclosedComment)
                })?;
            "*/".parse_next(input)?;
        } else {
            return Ok(());
        }
    }
}

fn parse_paren_group<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<Command<'input>>> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let (group_contents, _) = repeat_till(0.., parse_command, ')').parse_next(input)?;
    Ok(group_contents)
}

fn parse_command<'input>(input: &mut &'input BStr) -> winnow::Result<Command<'input>> {
    let command_str = parse_token(input)?;

    skip_comments_and_whitespace(input)?;

    let command = match command_str {
        b"GROUP" | b"INPUT" => Command::Group(parse_paren_group(input)?),
        b"OUTPUT_FORMAT" => Command::OutputFormat(parse_output_format(input)?),
        b"OUTPUT_ARCH" => {
            parse_paren_group(input)?;
            Command::Ignored
        }
        b"AS_NEEDED" => Command::AsNeeded(parse_paren_group(input)?),
        b"SECTIONS" => Command::Sections(parse_sections(input)?),
        b"ENTRY" => Command::Entry(parse_entry(input)?),
        b"VERSION" => Command::Version(parse_version(input)?),
        b"PROVIDE" => Command::Provide(parse_provide(input, false)?),
        b"PROVIDE_HIDDEN" => Command::Provide(parse_provide(input, true)?),
        b"ASSERT" => {
            let assert = parse_assert(input)?;
            opt(';').parse_next(input)?;
            skip_comments_and_whitespace(input)?;
            Command::Assert(assert)
        }
        b"MEMORY" => Command::Memory(parse_memory(input)?),
        b"PHDRS" => Command::Phdrs(parse_phdrs(input)?),
        b"INCLUDE" => {
            let path = parse_include_path(input)?;
            Command::Include(path)
        }
        other => {
            if let Some(op) = opt(parse_assignment_op).parse_next(input)? {
                // Symbol definition
                skip_comments_and_whitespace(input)?;
                let value = parse_expression.parse_next(input)?;
                skip_comments_and_whitespace(input)?;
                opt(';').parse_next(input)?;
                let expanded = op.expand(other, value);
                if other == b"." {
                    Command::SetLocation(Location { address: expanded })
                } else {
                    Command::SymbolDefinition {
                        name: other,
                        value: expanded,
                    }
                }
            } else {
                Command::Arg(other)
            }
        }
    };

    skip_comments_and_whitespace(input)?;

    Ok(command)
}

fn parse_provide<'input>(
    input: &mut &'input BStr,
    hidden: bool,
) -> winnow::Result<ProvideSymbolDefinition<'input>> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    '='.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let value = parse_expression.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(ProvideSymbolDefinition {
        name,
        value,
        hidden,
    })
}

fn parse_assert<'input>(input: &mut &'input BStr) -> winnow::Result<AssertCommand<'input>> {
    let remainder: &'input [u8] = input;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse expression using winnow - it will consume as much as it can
    let expression = parse_expression.parse_next(input)?;

    skip_comments_and_whitespace(input)?;
    ','.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse message (quoted string)
    let message = parse_token(input)?;

    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(AssertCommand {
        expression: Box::new(expression),
        message,
        remainder,
    })
}

fn parse_include_path<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    skip_comments_and_whitespace(input)?;
    let has_paren = opt('(').parse_next(input)?.is_some();
    skip_comments_and_whitespace(input)?;
    let path = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    if has_paren {
        ')'.parse_next(input)?;
        skip_comments_and_whitespace(input)?;
    }
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(path)
}

fn parse_memory_flags(input: &mut &BStr) -> winnow::Result<MemoryFlags> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let flags_bytes = take_while(0.., |b: u8| b != b')').parse_next(input)?;
    ')'.parse_next(input)?;
    let mut flags = MemoryFlags::default();
    for &b in flags_bytes {
        match b {
            b'r' | b'R' => flags.read = true,
            b'w' | b'W' => flags.write = true,
            b'x' | b'X' => flags.exec = true,
            b'a' | b'A' => flags.alloc = true,
            _ => {}
        }
    }
    Ok(flags)
}

fn parse_memory_region<'input>(input: &mut &'input BStr) -> winnow::Result<MemoryRegion<'input>> {
    let name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;

    let flags = if input.starts_with(b"(") {
        let flags = parse_memory_flags(input)?;
        skip_comments_and_whitespace(input)?;
        Some(flags)
    } else {
        None
    };

    // Parse the colon separator
    ':'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse the Origin block
    alt(("ORIGIN", "org", "o")).parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '='.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let origin = parse_expression.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse the comma separator
    ','.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse the Length block
    alt(("LENGTH", "len", "l")).parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '='.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let length = parse_expression.parse_next(input)?;

    Ok(MemoryRegion {
        name,
        origin,
        length,
        flags,
    })
}

fn parse_memory<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<MemoryRegion<'input>>> {
    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let (regions, _) = repeat_till(0.., parse_memory_region, '}').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(regions)
}

fn parse_phdr<'input>(input: &mut &'input BStr) -> winnow::Result<Phdr<'input>> {
    let name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;

    let ptype = if input.starts_with(b"PT_") {
        let ptype_str = parse_token(input)?;

        match ptype_str {
            b"PT_NULL" => Expression::Number(object::elf::PT_NULL.into_inner().into()),
            b"PT_LOAD" => Expression::Number(object::elf::PT_LOAD.into_inner().into()),
            b"PT_DYNAMIC" => Expression::Number(object::elf::PT_DYNAMIC.into_inner().into()),
            b"PT_INTERP" => Expression::Number(object::elf::PT_INTERP.into_inner().into()),
            b"PT_NOTE" => Expression::Number(object::elf::PT_NOTE.into_inner().into()),
            b"PT_SHLIB" => Expression::Number(object::elf::PT_SHLIB.into_inner().into()),
            b"PT_PHDR" => Expression::Number(object::elf::PT_PHDR.into_inner().into()),
            b"PT_TLS" => Expression::Number(object::elf::PT_TLS.into_inner().into()),
            b"PT_GNU_EH_FRAME" => {
                Expression::Number(object::elf::PT_GNU_EH_FRAME.into_inner().into())
            }
            b"PT_GNU_STACK" => Expression::Number(object::elf::PT_GNU_STACK.into_inner().into()),
            b"PT_GNU_RELRO" => Expression::Number(object::elf::PT_GNU_RELRO.into_inner().into()),
            b"PT_GNU_PROPERTY" => {
                Expression::Number(object::elf::PT_GNU_PROPERTY.into_inner().into())
            }
            b"PT_GNU_SFRAME" => Expression::Number(object::elf::PT_GNU_SFRAME.into_inner().into()),
            _ => {
                return Err(ContextError::default());
            }
        }
    } else {
        parse_expression.parse_next(input)?
    };

    skip_comments_and_whitespace(input)?;

    let mut flags = None;
    let mut has_filehdr = false;
    let mut has_phdrs = false;
    let mut at_address = None;
    while let Some(prefix) = opt(alt((b"FLAGS", b"FILEHDR", b"PHDRS", b"AT"))).parse_next(input)? {
        skip_comments_and_whitespace(input)?;
        match prefix {
            b"FLAGS" => {
                '('.parse_next(input)?;
                flags = Some(parse_expression.parse_next(input)?);
                ')'.parse_next(input)?;
            }
            b"FILEHDR" => has_filehdr = true,
            b"PHDRS" => has_phdrs = true,
            b"AT" => {
                at_address = Some(parse_at_address.parse_next(input)?);
            }
            _ => unreachable!(),
        }
        skip_comments_and_whitespace(input)?;
    }

    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(Phdr {
        name,
        ptype,
        flags,
        has_filehdr,
        has_phdrs,
        at_address,
    })
}

fn parse_phdrs<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<Phdr<'input>>> {
    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let (phdrs, _) = repeat_till(0.., parse_phdr, '}').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(phdrs)
}

fn parse_output_format<'input>(input: &mut &'input BStr) -> winnow::Result<OutputFormat<'input>> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let default = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    let mut big = None;
    let mut little = None;
    if opt(",").parse_next(input)?.is_some() {
        skip_comments_and_whitespace(input)?;
        big = Some(parse_token(input)?);
        skip_comments_and_whitespace(input)?;
        ",".parse_next(input)?;
        skip_comments_and_whitespace(input)?;
        little = Some(parse_token(input)?);
        skip_comments_and_whitespace(input)?;
    }
    ')'.parse_next(input)?;

    Ok(OutputFormat {
        default,
        big,
        little,
    })
}

/// Parse an expression - entry point for expression parsing
pub(crate) fn parse_expression<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    parse_ternary.parse_next(input)
}

pub(crate) fn parse_ternary<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_logical_or.parse_next(input)?;

    multispace0.parse_next(input)?;

    if opt('?').parse_next(input)?.is_some() {
        multispace0.parse_next(input)?;
        let true_expr = parse_ternary.parse_next(input)?;
        multispace0.parse_next(input)?;
        ':'.parse_next(input)?;
        multispace0.parse_next(input)?;
        let false_expr = parse_ternary.parse_next(input)?;
        left = Expression::Ternary(Box::new(left), Box::new(true_expr), Box::new(false_expr));
        multispace0.parse_next(input)?;
    }
    Ok(left)
}

/// Parse logical OR: expression || expression
fn parse_logical_or<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_logical_and.parse_next(input)?;

    multispace0.parse_next(input)?;

    while opt("||").parse_next(input)?.is_some() {
        multispace0.parse_next(input)?;
        let right = parse_logical_and.parse_next(input)?;
        left = Expression::LogicalOr(Box::new(left), Box::new(right));
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse logical AND: expression && expression
fn parse_logical_and<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_comparison.parse_next(input)?;

    multispace0.parse_next(input)?;

    while opt("&&").parse_next(input)?.is_some() {
        multispace0.parse_next(input)?;
        let right = parse_comparison.parse_next(input)?;
        left = Expression::LogicalAnd(Box::new(left), Box::new(right));
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse comparison expression: expression < expression, expression == expression, etc.
fn parse_comparison<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_bitwise_or.parse_next(input)?;

    multispace0.parse_next(input)?;

    if let Some(op) = opt(alt((
        "<=".map(|_| CompOp::LessEqual),
        ">=".map(|_| CompOp::GreaterEqual),
        "==".map(|_| CompOp::Equal),
        "!=".map(|_| CompOp::NotEqual),
        '<'.map(|_| CompOp::LessThan),
        '>'.map(|_| CompOp::GreaterThan),
    )))
    .parse_next(input)?
    {
        multispace0.parse_next(input)?;
        let right = parse_bitwise_or.parse_next(input)?;
        left = match op {
            CompOp::LessThan => Expression::LessThan(Box::new(left), Box::new(right)),
            CompOp::GreaterThan => Expression::GreaterThan(Box::new(left), Box::new(right)),
            CompOp::LessEqual => Expression::LessEqual(Box::new(left), Box::new(right)),
            CompOp::GreaterEqual => Expression::GreaterEqual(Box::new(left), Box::new(right)),
            CompOp::Equal => Expression::Equal(Box::new(left), Box::new(right)),
            CompOp::NotEqual => Expression::NotEqual(Box::new(left), Box::new(right)),
        };
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse Shift operators: <<, >>
fn parse_shift<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_additive.parse_next(input)?;

    multispace0.parse_next(input)?;

    while let Some(op) = opt(alt((
        "<<".map(|_| ShiftOp::Left),
        ">>".map(|_| ShiftOp::Right),
    )))
    .parse_next(input)?
    {
        multispace0.parse_next(input)?;
        let right = parse_additive.parse_next(input)?;
        left = match op {
            ShiftOp::Left => Expression::LeftShift(Box::new(left), Box::new(right)),
            ShiftOp::Right => Expression::RightShift(Box::new(left), Box::new(right)),
        };
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse bitwise OR: expression | expression
fn parse_bitwise_or<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_bitwise_xor.parse_next(input)?;

    multispace0.parse_next(input)?;

    while opt(('|', winnow::combinator::not('|')))
        .parse_next(input)?
        .is_some()
    {
        multispace0.parse_next(input)?;
        let right = parse_bitwise_xor.parse_next(input)?;
        left = Expression::BitwiseOr(Box::new(left), Box::new(right));
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse bitwise XOR: expression ^ expression
fn parse_bitwise_xor<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_bitwise_and.parse_next(input)?;

    multispace0.parse_next(input)?;

    while opt('^').parse_next(input)?.is_some() {
        multispace0.parse_next(input)?;
        let right = parse_bitwise_and.parse_next(input)?;
        left = Expression::BitwiseXor(Box::new(left), Box::new(right));
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse bitwise AND: expression & expression
fn parse_bitwise_and<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_shift.parse_next(input)?;

    multispace0.parse_next(input)?;

    while opt(('&', winnow::combinator::not('&')))
        .parse_next(input)?
        .is_some()
    {
        multispace0.parse_next(input)?;
        let right = parse_shift.parse_next(input)?;
        left = Expression::BitwiseAnd(Box::new(left), Box::new(right));
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse additive operators: +, -
fn parse_additive<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_multiplicative.parse_next(input)?;

    multispace0.parse_next(input)?;

    while let Some(op) =
        opt(alt(('+'.map(|_| AddOp::Add), '-'.map(|_| AddOp::Subtract)))).parse_next(input)?
    {
        multispace0.parse_next(input)?;
        let right = parse_multiplicative.parse_next(input)?;
        left = match op {
            AddOp::Add => Expression::Add(Box::new(left), Box::new(right)),
            AddOp::Subtract => Expression::Subtract(Box::new(left), Box::new(right)),
        };
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse multiplicative operators: *, /
fn parse_multiplicative<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let mut left = parse_unary.parse_next(input)?;

    multispace0.parse_next(input)?;

    while let Some(op) = opt(alt((
        '*'.map(|_| MulOp::Multiply),
        '/'.map(|_| MulOp::Divide),
        '%'.map(|_| MulOp::Modulo),
    )))
    .parse_next(input)?
    {
        multispace0.parse_next(input)?;
        let right = parse_unary.parse_next(input)?;
        left = match op {
            MulOp::Multiply => Expression::Multiply(Box::new(left), Box::new(right)),
            MulOp::Divide => Expression::Divide(Box::new(left), Box::new(right)),
            MulOp::Modulo => Expression::Modulo(Box::new(left), Box::new(right)),
        };
        multispace0.parse_next(input)?;
    }

    Ok(left)
}

/// Parse unary prefix operators: !, ~, -
fn parse_unary<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    multispace0.parse_next(input)?;

    if opt(('!', winnow::combinator::not('=')))
        .parse_next(input)?
        .is_some()
    {
        let operand = parse_unary.parse_next(input)?;
        return Ok(Expression::LogicalNot(Box::new(operand)));
    }

    if opt('~').parse_next(input)?.is_some() {
        let operand = parse_unary.parse_next(input)?;
        return Ok(Expression::BitwiseNot(Box::new(operand)));
    }

    if opt('-').parse_next(input)?.is_some() {
        let operand = parse_unary.parse_next(input)?;
        return Ok(Expression::Negate(Box::new(operand)));
    }

    parse_primary.parse_next(input)
}

/// Parse hex and decimal numbers, applying an optional K (x1024) or M (x1024^2) suffix.
fn parse_number_with_suffix<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    let base_number = alt((
        // Hex numbers (0x or 0X prefix)
        preceded(alt(("0x", "0X")), hex_uint::<_, u64, _>),
        // Decimal numbers
        dec_uint::<_, u64, _>,
    ))
    .parse_next(input)?;

    let suffix = opt(one_of(b"KkMm")).parse_next(input)?;

    let final_value = match suffix {
        Some(b'K' | b'k') => base_number.wrapping_mul(1024),
        Some(b'M' | b'm') => base_number.wrapping_mul(1024 * 1024),
        _ => base_number,
    };

    Ok(Expression::Number(final_value))
}

/// Parse primary expressions: numbers, symbols, functions, parentheses
fn parse_primary<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    multispace0.parse_next(input)?;

    alt((
        // Parentheses - parse expression inside
        delimited('(', parse_expression, ')'),
        // Numbers (hex/decimal) with optional size suffixes
        parse_number_with_suffix,
        // Functions and symbols (identifiers) - this handles '.' as well
        parse_identifier_or_function,
    ))
    .parse_next(input)
}

/// Parse an identifier (symbol or function call)
fn parse_identifier_or_function<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
    // Parse identifier: starts with letter or underscore, contains alphanumeric, underscore, or dot
    let ident = take_while(1.., |b: u8| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
    })
    .verify(|s: &[u8]| {
        // Must start with letter, underscore, or dot
        s[0].is_ascii_alphabetic() || s[0] == b'_' || s[0] == b'.'
    })
    .parse_next(input)?;

    // Special case: if it's just '.', it's the location counter
    if ident == b"." {
        return Ok(Expression::LocationCounter);
    }

    multispace0.parse_next(input)?;

    // Check if it's a function call
    if input.starts_with(b"(") {
        multispace0.parse_next(input)?;

        match ident {
            b"SIZEOF" => {
                let arg = parse_function_arg.parse_next(input)?;
                Ok(Expression::Sizeof(arg))
            }
            b"ALIGNOF" => {
                let arg = parse_function_arg.parse_next(input)?;
                Ok(Expression::Alignof(arg))
            }
            b"ADDR" => {
                let arg = parse_function_arg.parse_next(input)?;
                Ok(Expression::Addr(arg))
            }
            b"ORIGIN" => {
                let arg = parse_function_arg.parse_next(input)?;
                Ok(Expression::Origin(arg))
            }
            b"LENGTH" => {
                let arg = parse_function_arg.parse_next(input)?;
                Ok(Expression::Length(arg))
            }
            b"LOADADDR" => {
                let arg = parse_function_arg.parse_next(input)?;
                Ok(Expression::Loadaddr(arg))
            }
            b"ALIGN" => {
                '('.parse_next(input)?;
                let arg_0 = parse_expression.parse_next(input)?;
                multispace0.parse_next(input)?;
                let arg_1 = if opt(',').parse_next(input)?.is_some() {
                    multispace0.parse_next(input)?;
                    let arg_1 = parse_expression.parse_next(input)?;
                    multispace0.parse_next(input)?;
                    Some(arg_1)
                } else {
                    None
                };
                ')'.parse_next(input)?;
                if let Some(arg_1) = arg_1 {
                    Ok(Expression::Align(Box::new(arg_1), Some(Box::new(arg_0))))
                } else {
                    Ok(Expression::Align(Box::new(arg_0), None))
                }
            }
            b"MIN" => {
                // MIN takes two expressions separated by comma
                '('.parse_next(input)?;
                let first = parse_expression.parse_next(input)?;
                multispace0.parse_next(input)?;
                ','.parse_next(input)?;
                multispace0.parse_next(input)?;
                let second = parse_expression.parse_next(input)?;
                multispace0.parse_next(input)?;
                ')'.parse_next(input)?;
                Ok(Expression::Min(Box::new(first), Box::new(second)))
            }
            b"MAX" => {
                // MAX takes two expressions separated by comma
                '('.parse_next(input)?;
                let first = parse_expression.parse_next(input)?;
                multispace0.parse_next(input)?;
                ','.parse_next(input)?;
                multispace0.parse_next(input)?;
                let second = parse_expression.parse_next(input)?;
                multispace0.parse_next(input)?;
                ')'.parse_next(input)?;
                Ok(Expression::Max(Box::new(first), Box::new(second)))
            }
            b"SEGMENT_START" => {
                '('.parse_next(input)?;
                multispace0.parse_next(input)?;
                '"'.parse_next(input)?;
                let name = take_while(1.., |b: u8| b != b'"')
                    .verify(|s: &[u8]| !s.is_empty())
                    .parse_next(input)?;
                '"'.parse_next(input)?;
                multispace0.parse_next(input)?;
                ','.parse_next(input)?;
                multispace0.parse_next(input)?;
                let default_expr = parse_expression.parse_next(input)?;
                multispace0.parse_next(input)?;
                ')'.parse_next(input)?;
                let segment_name = crate::parsing::SegmentName::from_bytes(name);
                Ok(Expression::SegmentStart(
                    segment_name,
                    Box::new(default_expr),
                ))
            }
            b"DEFINED" => {
                let symbol = parse_function_arg.parse_next(input)?;
                Ok(Expression::Defined(symbol))
            }
            b"ABSOLUTE" => {
                '('.parse_next(input)?;
                skip_comments_and_whitespace(input)?;
                let inner = parse_expression.parse_next(input)?;
                skip_comments_and_whitespace(input)?;
                ')'.parse_next(input)?;
                Ok(inner)
            }
            b"ASSERT" => {
                let assert = parse_assert.parse_next(input)?;
                Ok(Expression::Assert(assert))
            }
            _ => Err(ContextError::default()),
        }
    } else if ident == b"SIZEOF_HEADERS" {
        Ok(Expression::SizeofHeaders)
    } else {
        // It's a symbol
        Ok(Expression::Symbol(ident))
    }
}

/// Parse a function argument (section name for SIZEOF/ADDR)
fn parse_function_arg<'a>(input: &mut &'a BStr) -> winnow::Result<&'a [u8]> {
    '('.parse_next(input)?;
    multispace0.parse_next(input)?;

    // Section names: start with '.', letter, or underscore
    let arg = take_while(1.., |b: u8| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
    })
    .verify(|s: &[u8]| s[0] == b'.' || s[0].is_ascii_alphabetic() || s[0] == b'_')
    .parse_next(input)?;

    multispace0.parse_next(input)?;
    ')'.parse_next(input)?;

    Ok(arg)
}

#[derive(Debug, Clone, Copy)]
enum CompOp {
    LessThan,
    GreaterThan,
    LessEqual,
    GreaterEqual,
    Equal,
    NotEqual,
}

#[derive(Debug, Clone, Copy)]
enum ShiftOp {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy)]
enum AddOp {
    Add,
    Subtract,
}

#[derive(Debug, Clone, Copy)]
enum MulOp {
    Multiply,
    Divide,
    Modulo,
}

#[derive(Debug, Clone, Copy)]
enum AssignmentOp {
    Assign,
    Add,
    Subtract,
    Multiply,
    Divide,
    LeftShift,
    RightShift,
    BitwiseAnd,
    BitwiseOr,
    BitwiseXor,
}

fn parse_assignment_op(input: &mut &BStr) -> winnow::Result<AssignmentOp> {
    alt((
        alt((
            "+=".value(AssignmentOp::Add),
            "-=".value(AssignmentOp::Subtract),
            "*=".value(AssignmentOp::Multiply),
            "/=".value(AssignmentOp::Divide),
            "<<=".value(AssignmentOp::LeftShift),
        )),
        alt((
            ">>=".value(AssignmentOp::RightShift),
            "&=".value(AssignmentOp::BitwiseAnd),
            "|=".value(AssignmentOp::BitwiseOr),
            "^=".value(AssignmentOp::BitwiseXor),
            ("=", winnow::combinator::not('=')).value(AssignmentOp::Assign),
        )),
    ))
    .parse_next(input)
}

impl AssignmentOp {
    fn expand<'a>(self, name: &'a [u8], rhs: Expression<'a>) -> Expression<'a> {
        let lhs = if name == b"." {
            Expression::LocationCounter
        } else {
            Expression::Symbol(name)
        };
        match self {
            AssignmentOp::Assign => rhs,
            AssignmentOp::Add => Expression::Add(Box::new(lhs), Box::new(rhs)),
            AssignmentOp::Subtract => Expression::Subtract(Box::new(lhs), Box::new(rhs)),
            AssignmentOp::Multiply => Expression::Multiply(Box::new(lhs), Box::new(rhs)),
            AssignmentOp::Divide => Expression::Divide(Box::new(lhs), Box::new(rhs)),
            AssignmentOp::LeftShift => Expression::LeftShift(Box::new(lhs), Box::new(rhs)),
            AssignmentOp::RightShift => Expression::RightShift(Box::new(lhs), Box::new(rhs)),
            AssignmentOp::BitwiseAnd => Expression::BitwiseAnd(Box::new(lhs), Box::new(rhs)),
            AssignmentOp::BitwiseOr => Expression::BitwiseOr(Box::new(lhs), Box::new(rhs)),
            AssignmentOp::BitwiseXor => Expression::BitwiseXor(Box::new(lhs), Box::new(rhs)),
        }
    }
}

fn parse_commands<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<Command<'input>>> {
    skip_comments_and_whitespace(input)?;

    Ok(repeat_till(0.., parse_command, eof).parse_next(input)?.0)
}

fn expand_commands<'data>(
    commands: Vec<Command<'data>>,
    load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    stack: &mut Vec<Vec<u8>>,
) -> Result<Vec<Command<'data>>> {
    let mut out = Vec::with_capacity(commands.len());
    for cmd in commands {
        match cmd {
            Command::Include(path) => {
                let included = load_included_script(path, load, stack)?;
                out.extend(included);
            }
            Command::Sections(mut sections) => {
                sections.commands =
                    expand_section_commands(std::mem::take(&mut sections.commands), load, stack)?;
                out.push(Command::Sections(sections));
            }
            Command::Group(inner) => {
                out.push(Command::Group(expand_commands(inner, load, stack)?));
            }
            Command::AsNeeded(inner) => {
                out.push(Command::AsNeeded(expand_commands(inner, load, stack)?));
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn expand_section_commands<'data>(
    commands: Vec<SectionCommand<'data>>,
    load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    stack: &mut Vec<Vec<u8>>,
) -> Result<Vec<SectionCommand<'data>>> {
    let mut out = Vec::with_capacity(commands.len());
    for cmd in commands {
        match cmd {
            SectionCommand::Include(path) => {
                out.extend(load_included_section_commands(path, load, stack)?);
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn load_included_script<'data>(
    path: &[u8],
    load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    stack: &mut Vec<Vec<u8>>,
) -> Result<Vec<Command<'data>>> {
    push_include_path(path, stack)?;
    let bytes = load(path)?;
    let parsed = parse_commands
        .parse(BStr::new(bytes))
        .map_err(|error| error!("Failed to parse included linker script:\n{error}"))?;
    let expanded = expand_commands(parsed, load, stack)?;
    stack.pop();
    Ok(expanded)
}

fn load_included_section_commands<'data>(
    path: &[u8],
    load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    stack: &mut Vec<Vec<u8>>,
) -> Result<Vec<SectionCommand<'data>>> {
    push_include_path(path, stack)?;
    let bytes = load(path)?;
    let section_cmds = match parse_section_command_list.parse(BStr::new(bytes)) {
        Ok(cmds) => cmds,
        Err(_) => {
            let parsed = parse_commands
                .parse(BStr::new(bytes))
                .map_err(|error| error!("Failed to parse included linker script:\n{error}"))?;
            section_commands_from_top_level(parsed)?
        }
    };
    let expanded = expand_section_commands(section_cmds, load, stack)?;
    stack.pop();
    Ok(expanded)
}

fn section_commands_from_top_level<'data>(
    commands: Vec<Command<'data>>,
) -> Result<Vec<SectionCommand<'data>>> {
    let mut out = Vec::new();
    for cmd in commands {
        match cmd {
            Command::Sections(sections) => out.extend(sections.commands),
            Command::SetLocation(loc) => out.push(SectionCommand::SetLocation(loc)),
            Command::Assert(assert_cmd) => out.push(SectionCommand::Assert(assert_cmd)),
            Command::Provide(provide) => out.push(SectionCommand::Provide(provide)),
            Command::SymbolDefinition { name, value } => {
                out.push(SectionCommand::SymbolAssignment(SymbolAssignment {
                    name,
                    expr: value,
                }));
            }
            Command::Include(path) => out.push(SectionCommand::Include(path)),
            Command::Ignored => {}
            Command::Arg(name) => {
                crate::bail!(
                    "INCLUDE inside SECTIONS cannot contain `{}`",
                    String::from_utf8_lossy(name)
                );
            }
            _ => {
                crate::bail!("INCLUDE inside SECTIONS cannot contain that top-level command");
            }
        }
    }
    Ok(out)
}

fn push_include_path(path: &[u8], stack: &mut Vec<Vec<u8>>) -> Result {
    if stack.iter().any(|p| p == path) {
        crate::bail!("cyclic INCLUDE of `{}`", String::from_utf8_lossy(path));
    }
    stack.push(path.to_vec());
    Ok(())
}

fn parse_section_command_list<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<Vec<SectionCommand<'input>>> {
    skip_comments_and_whitespace(input)?;
    Ok(repeat_till(0.., parse_section_command, eof)
        .parse_next(input)?
        .0)
}

fn parse_entry<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let symbol_name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    Ok(symbol_name)
}

fn parse_version<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    skip_comments_and_whitespace(input)?;
    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let mut brace_count = 1;
    let mut pos = 0;

    while brace_count > 0 && pos < input.len() {
        match input[pos] {
            b'{' => brace_count += 1,
            b'}' => brace_count -= 1,
            _ => {}
        }
        pos += 1;
    }

    if brace_count != 0 {
        return Err(ContextError::new());
    }

    let version_content = &input[..pos - 1];
    *input = &input[pos..];

    skip_comments_and_whitespace(input)?;

    opt(';').parse_next(input)?;

    Ok(version_content)
}

fn parse_sections<'input>(input: &mut &'input BStr) -> winnow::Result<Sections<'input>> {
    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let (commands, _) = repeat_till(0.., parse_section_command, '}').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(Sections { commands })
}

fn parse_section_command<'input>(
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

    while !input.starts_with(b"{") {
        skip_comments_and_whitespace(input)?;
        if input.starts_with(b"AT>") {
            break;
        }
        if opt("AT").parse_next(input)?.is_some() {
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
    }))
}

fn parse_overlay<'input>(
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

fn parse_section_attribute(input: &mut &BStr) -> winnow::Result<SectionAttributes> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let section_type = parse_token.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let section_type = match section_type {
        b"NOLOAD" => SectionAttributes::Noload,
        b"READONLY" => SectionAttributes::Readonly,
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

fn parse_fill<'input>(input: &mut &'input BStr) -> winnow::Result<Fill<'input>> {
    return Ok(Fill {
        value: parse_expression.parse_next(input)?,
    });
}

fn parse_alignment(input: &mut &BStr) -> winnow::Result<Alignment> {
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

fn parse_at_address<'input>(input: &mut &'input BStr) -> winnow::Result<Expression<'input>> {
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let address = parse_expression.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(address)
}

fn parse_contents_command<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    alt((
        parse_contents_provide,
        parse_contents_assert,
        parse_contents_fill,
        parse_output_data,
        parse_matcher,
        parse_assignment,
        parse_constructors,
    ))
    .parse_next(input)
}

fn parse_contents_fill<'input>(
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

fn parse_output_data<'input>(input: &mut &'input BStr) -> winnow::Result<ContentsCommand<'input>> {
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

fn parse_contents_provide<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    let hidden = alt(("PROVIDE_HIDDEN", "PROVIDE")).parse_next(input)? == b"PROVIDE_HIDDEN";
    skip_comments_and_whitespace(input)?;
    let provide = parse_provide(input, hidden)?;
    Ok(ContentsCommand::Provide(provide))
}

fn parse_assignment<'input>(input: &mut &'input BStr) -> winnow::Result<ContentsCommand<'input>> {
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

fn parse_constructors<'input>(input: &mut &'input BStr) -> winnow::Result<ContentsCommand<'input>> {
    "CONSTRUCTORS".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(ContentsCommand::Constructors)
}

fn parse_matcher<'input>(input: &mut &'input BStr) -> winnow::Result<ContentsCommand<'input>> {
    let matcher = alt((parse_keep, parse_matcher_pattern)).parse_next(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(ContentsCommand::Matcher(matcher))
}

fn parse_keep<'input>(input: &mut &'input BStr) -> winnow::Result<Matcher<'input>> {
    "KEEP".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    let mut matcher = parse_matcher_pattern(input)?;
    matcher.must_keep = true;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(matcher)
}

fn parse_exclude_file_list<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<&'input [u8]>> {
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

fn parse_matcher_pattern<'input>(input: &mut &'input BStr) -> winnow::Result<Matcher<'input>> {
    let mut exclude_file_patterns = Vec::new();
    if input.starts_with(b"EXCLUDE_FILE") {
        exclude_file_patterns = parse_exclude_file_list(input)?;
    }

    // Parse the file pattern token (e.g., *, foo.o, *crtbegin*.o).
    let file_pattern = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    if input.starts_with(b"EXCLUDE_FILE") {
        exclude_file_patterns.extend(parse_exclude_file_list(input)?);
    }

    let (patterns, _) = repeat_till(0.., parse_pattern, ')').parse_next(input)?;
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

fn parse_sort(input: &mut &BStr) -> winnow::Result<SortKind> {
    alt((
        "SORT_BY_INIT_PRIORITY".map(|_| SortKind::InitPriority),
        "SORT_BY_NAME".map(|_| SortKind::Name),
        "SORT_BY_ALIGNMENT".map(|_| SortKind::Alignment),
        "SORT".map(|_| SortKind::Name),
    ))
    .parse_next(input)
}

fn parse_pattern<'input>(input: &mut &'input BStr) -> winnow::Result<SectionPattern<'input>> {
    let sort = opt(parse_sort).parse_next(input)?.unwrap_or(SortKind::None);

    if sort != SortKind::None {
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

    if sort != SortKind::None {
        ')'.parse_next(input)?;
        skip_comments_and_whitespace(input)?;
    }

    Ok(SectionPattern { name, sort })
}

fn parse_contents_assert<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<ContentsCommand<'input>> {
    "ASSERT".parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let assert = parse_assert(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(ContentsCommand::Assert(assert))
}

/// Call `cb` for each input file requested by `commands`.
fn foreach_input(
    commands: &[Command],
    modifiers: Modifiers,
    cb: &mut impl FnMut(Input) -> Result,
) -> Result {
    for command in commands {
        match command {
            Command::Arg(arg) => {
                let spec = if let Some(lib_name) = arg.strip_prefix(b"-l") {
                    InputSpec::Lib(Box::from(to_str(lib_name)?))
                } else {
                    InputSpec::File(Box::from(Path::new(to_str(arg)?)))
                };
                cb(Input {
                    spec,
                    search_first: None,
                    modifiers,
                })?;
            }
            Command::Group(subs) => foreach_input(subs, modifiers, cb)?,
            Command::AsNeeded(subs) => {
                let sub_modifiers = Modifiers {
                    as_needed: true,
                    ..modifiers
                };
                foreach_input(subs, sub_modifiers, cb)?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn to_str(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes)
        .with_context(|| format!("Expected UTF-8, found `{}`", String::from_utf8_lossy(bytes)))
}

#[derive(Debug)]
enum LinkerScriptError {
    InvalidAlignment,
    UnclosedComment,
    UnsupportedNestedSort,
}

impl std::error::Error for LinkerScriptError {}

impl std::fmt::Display for LinkerScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkerScriptError::InvalidAlignment => write!(f, "Invalid alignment"),
            LinkerScriptError::UnclosedComment => write!(f, "Unclosed comment"),
            LinkerScriptError::UnsupportedNestedSort => write!(
                f,
                "Nested sorting commands in linker scripts is not supported"
            ),
        }
    }
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
