use crate::linker_script::Expression;
use hashbrown::HashMap;
use wild_error::bail;
use wild_error::error::Result;

pub fn evaluate_const<'data>(expr: &Expression<'data>) -> Result<u64> {
    evaluate_const_with_symbols(expr, &HashMap::new())
}

/// Like [`evaluate_const`], but `Expression::Symbol` may resolve through `known`.
/// Used to fold chains of constant script assignments (`later = BASE + OFFSET`)
/// before layout, matching GNU ld's forward constant references.
pub fn evaluate_const_with_symbols<'data>(
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
