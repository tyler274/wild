//! Evaluation of linker script ASSERT commands and location-counter expressions.

mod early;
mod value;

#[allow(unused_imports)]
pub use early::*;
#[allow(unused_imports)]
pub use value::*;
pub use wild_scripts::{evaluate_const, evaluate_const_with_symbols};
