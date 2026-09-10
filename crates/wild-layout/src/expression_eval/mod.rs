//! Evaluation of linker script ASSERT commands and location-counter expressions.

mod early;
mod value;

#[allow(unused_imports)]
pub(crate) use early::*;
#[allow(unused_imports)]
pub(crate) use value::*;
pub(crate) use wild_scripts::evaluate_const;
pub(crate) use wild_scripts::evaluate_const_with_symbols;
