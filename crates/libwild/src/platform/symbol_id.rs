use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use wild_error::error::Context as _;
use wild_util::sharding::ShardKey;

/// An ID for a symbol. All symbols from all input files are allocated a unique symbol ID. The
/// symbol ID 0 is reserved for the undefined symbol.
#[derive(Clone, Copy, derive_more::Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[debug("sym-{_0}")]
pub(crate) struct SymbolId(pub(crate) u32);

pub(crate) struct AtomicSymbolId(AtomicU32);

/// A range of symbol IDs that are defined by the same input file.
///
/// This exists to translate between 3 different ways of identifying a symbol:
/// - A `SymbolId` is a globally unique identifier for a symbol.
/// - An `object::SymbolIndex` is an index into the ELF symbol table of the input file.
/// - A `usize` offset is an index into our own data structures for the file.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SymbolIdRange {
    start_symbol_id: SymbolId,
    pub(crate) num_symbols: usize,
}

impl SymbolIdRange {
    pub(crate) fn prelude(num_symbols: usize) -> SymbolIdRange {
        SymbolIdRange {
            start_symbol_id: SymbolId::undefined(),
            num_symbols,
        }
    }

    pub(crate) fn input(start_symbol_id: SymbolId, num_symbols: usize) -> SymbolIdRange {
        SymbolIdRange {
            start_symbol_id,
            num_symbols,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.num_symbols
    }

    pub(crate) fn start(&self) -> SymbolId {
        self.start_symbol_id
    }

    pub(crate) fn as_usize(&self) -> std::ops::Range<usize> {
        self.start_symbol_id.as_usize()..self.start_symbol_id.as_usize() + self.num_symbols
    }

    pub(crate) fn offset_to_id(&self, offset: usize) -> SymbolId {
        debug_assert!(offset < self.num_symbols);
        self.start_symbol_id.add_usize(offset)
    }

    pub(crate) fn id_to_offset(&self, symbol_id: SymbolId) -> usize {
        let offset = (symbol_id.0 - self.start_symbol_id.0) as usize;
        debug_assert!(
            offset < self.num_symbols,
            "{symbol_id} not within {}..{}",
            self.start_symbol_id.0,
            self.start_symbol_id.0 as usize + self.num_symbols
        );
        offset
    }

    pub(crate) fn offset_to_input(&self, offset: usize) -> object::SymbolIndex {
        debug_assert!(offset < self.num_symbols);
        object::SymbolIndex(offset)
    }

    pub(crate) fn input_to_offset(&self, symbol_index: object::SymbolIndex) -> usize {
        let offset = symbol_index.0;
        debug_assert!(offset < self.num_symbols);
        offset
    }

    pub(crate) fn input_to_id(&self, symbol_index: object::SymbolIndex) -> SymbolId {
        self.offset_to_id(self.input_to_offset(symbol_index))
    }

    pub(crate) fn id_to_input(&self, symbol_id: SymbolId) -> object::SymbolIndex {
        self.offset_to_input(self.id_to_offset(symbol_id))
    }

    /// Returns a range that covers from the start of `a` to the end of `b`.
    pub(crate) fn covering(a: SymbolIdRange, b: SymbolIdRange) -> SymbolIdRange {
        SymbolIdRange {
            start_symbol_id: a.start_symbol_id,
            num_symbols: b.start_symbol_id.as_usize() + b.len() - a.start_symbol_id.as_usize(),
        }
    }

    pub(crate) fn empty() -> SymbolIdRange {
        Self::input(SymbolId::from_usize(0), 0)
    }

    pub(crate) fn contains(&self, id: SymbolId) -> bool {
        self.start() <= id && id < self.start().add_usize(self.len())
    }
}

impl IntoIterator for SymbolIdRange {
    type Item = SymbolId;

    type IntoIter = SymbolIdRangeIterator;

    fn into_iter(self) -> Self::IntoIter {
        SymbolIdRangeIterator {
            remaining: self.len(),
            next: self.start_symbol_id,
        }
    }
}

pub(crate) struct SymbolIdRangeIterator {
    remaining: usize,
    next: SymbolId,
}

impl Iterator for SymbolIdRangeIterator {
    type Item = SymbolId;

    fn next(&mut self) -> Option<Self::Item> {
        self.remaining = self.remaining.checked_sub(1)?;
        let value = self.next;
        self.next = value.next();
        Some(value)
    }
}

impl SymbolId {
    pub(crate) fn undefined() -> Self {
        Self(0)
    }

    pub(crate) fn from_usize(value: usize) -> SymbolId {
        Self::new(u32::try_from(value).expect("Symbols overflowed 32 bits"))
    }

    pub(crate) fn as_usize(self) -> usize {
        self.0 as usize
    }

    const fn new(value: u32) -> SymbolId {
        SymbolId(value)
    }

    pub(crate) fn offset_from(self, base: SymbolId) -> usize {
        (self.0 - base.0) as usize
    }

    pub(crate) fn to_offset(self, range: SymbolIdRange) -> usize {
        range.id_to_offset(self)
    }

    pub(crate) fn to_input(self, range: SymbolIdRange) -> object::SymbolIndex {
        range.id_to_input(self)
    }

    pub(crate) fn is_undefined(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }

    pub(crate) fn as_atomic(self) -> AtomicSymbolId {
        AtomicSymbolId(AtomicU32::new(self.0))
    }
}

impl AtomicSymbolId {
    pub(crate) fn store(&self, selected: SymbolId) {
        self.0.store(selected.0, Ordering::Relaxed);
    }

    pub(crate) fn into_non_atomic(self) -> SymbolId {
        SymbolId(self.0.into_inner())
    }
}

impl TryFrom<usize> for SymbolId {
    type Error = wild_error::error::Error;

    fn try_from(value: usize) -> std::result::Result<Self, Self::Error> {
        Ok(SymbolId(u32::try_from(value).context("Too many symbols")?))
    }
}

impl std::fmt::Display for SymbolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl ShardKey for SymbolId {
    fn zero() -> Self {
        SymbolId(0)
    }

    fn add_usize(self, offset: usize) -> Self {
        SymbolId(
            (self.as_usize() + offset)
                .try_into()
                .expect("Symbol ID overflowed 32 bits"),
        )
    }
}
