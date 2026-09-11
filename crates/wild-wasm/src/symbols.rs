use std::ops::Range;
use wasmparser::SymbolFlags;

// NOTE: We deliberately don't reuse `wasmparser::SymbolInfo<'data>` here. It carries `&'data str`
// names, but `Platform::SymtabEntry` requires `Symbol: 'static + Copy`, so a wrapper around
// `SymbolInfo` would have to drop the borrowed strings anyway.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WasmSymbol {
    pub(crate) kind: WasmSymbolKind,
    pub(crate) flags: u32,
    pub(crate) index: u32,
    pub(crate) offset: u32,
    pub(crate) size: u32,
    pub(crate) name_start: u32,
    pub(crate) name_len: u32,
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub(crate) enum WasmSymbolKind {
    #[default]
    Null, // Doesn't correspond to any real wasm symbol kind.
    Func,
    Data,
    Global,
    Section,
    Event,
    Table,
}

impl WasmSymbol {
    pub(crate) fn raw_flags(&self) -> SymbolFlags {
        SymbolFlags::from_bits_truncate(self.flags)
    }

    pub(crate) fn is_undefined(&self) -> bool {
        self.raw_flags().contains(SymbolFlags::UNDEFINED)
    }

    pub(crate) fn is_weak(&self) -> bool {
        self.raw_flags().contains(SymbolFlags::BINDING_WEAK)
    }

    pub(crate) fn is_local(&self) -> bool {
        self.raw_flags().contains(SymbolFlags::BINDING_LOCAL)
    }

    pub(crate) fn is_hidden(&self) -> bool {
        self.raw_flags().contains(SymbolFlags::VISIBILITY_HIDDEN)
    }

    pub(crate) fn is_explicit_name(&self) -> bool {
        self.raw_flags().contains(SymbolFlags::EXPLICIT_NAME)
    }

    pub(crate) fn has_name(&self) -> bool {
        self.name_len != 0
    }

    pub(crate) fn name_range(&self) -> Range<usize> {
        let s = self.name_start as usize;
        s..s + self.name_len as usize
    }
}
