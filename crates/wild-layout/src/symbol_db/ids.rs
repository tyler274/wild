#[allow(unused_imports)]
pub(crate) use crate::platform::symbol_id::*;
use crate::symbol::UnversionedSymbolName;
use std::fmt::Display;

pub(crate) struct SymbolNameDisplay<'data> {
    pub(super) name: Option<UnversionedSymbolName<'data>>,
    pub(super) demangle: bool,
}

impl Display for SymbolNameDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(name) = self.name {
            if let Ok(s) = std::str::from_utf8(name.bytes()) {
                if self.demangle {
                    Display::fmt(&symbolic_demangle::demangle(s), f)
                } else {
                    Display::fmt(s, f)
                }
            } else {
                write!(f, "INVALID UTF-8({:?})", name.bytes())
            }
        } else {
            write!(f, "SYMBOL-READ-ERROR")
        }
    }
}
