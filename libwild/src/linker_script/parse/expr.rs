use super::*;
use crate::linker_script::ast::*;
use winnow::BStr;
use winnow::Parser as _;
use winnow::ascii::dec_uint;
use winnow::ascii::hex_uint;
use winnow::ascii::multispace0;
use winnow::combinator::alt;
use winnow::combinator::delimited;
use winnow::combinator::opt;
use winnow::combinator::preceded;
use winnow::error::ContextError;
use winnow::token::one_of;
use winnow::token::take_while;

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
pub(crate) fn parse_logical_or<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_logical_and<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_comparison<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_shift<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_bitwise_or<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_bitwise_xor<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_bitwise_and<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_additive<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_multiplicative<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_unary<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_number_with_suffix<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_primary<'a>(input: &mut &'a BStr) -> winnow::Result<Expression<'a>> {
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
pub(crate) fn parse_identifier_or_function<'a>(
    input: &mut &'a BStr,
) -> winnow::Result<Expression<'a>> {
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
                Ok(Expression::Absolute(Box::new(inner)))
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
pub(crate) fn parse_function_arg<'a>(input: &mut &'a BStr) -> winnow::Result<&'a [u8]> {
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
pub(crate) enum CompOp {
    LessThan,
    GreaterThan,
    LessEqual,
    GreaterEqual,
    Equal,
    NotEqual,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ShiftOp {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum AddOp {
    Add,
    Subtract,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MulOp {
    Multiply,
    Divide,
    Modulo,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum AssignmentOp {
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

pub(crate) fn parse_assignment_op(input: &mut &BStr) -> winnow::Result<AssignmentOp> {
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
    pub(crate) fn expand<'a>(self, name: &'a [u8], rhs: Expression<'a>) -> Expression<'a> {
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
