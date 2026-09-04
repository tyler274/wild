#[allow(unused_imports)]
use crate::elf::abi::*;
#[allow(unused_imports)]
use crate::elf::file::*;
#[allow(unused_imports)]
use crate::elf::gnu::*;
#[allow(unused_imports)]
use crate::elf::types::*;
use crate::error::Result;
use std::marker::PhantomData;
use std::ops::Range;

pub(crate) const fn relr_bitmap_slots<C: ElfClass>() -> u64 {
    C::RELR_ENTRY_SIZE * 8 - 1
}

pub(crate) struct RelrBitmap<C: ElfClass> {
    pub(crate) range: Range<u64>,
    pub(crate) encoded: u64,
    pub(crate) class: PhantomData<C>,
}

impl<C: ElfClass> RelrBitmap<C> {
    // Return bitmap starting after the current address.
    pub(crate) fn after(address: u64) -> Self {
        let start = address + C::RELR_ENTRY_SIZE;
        Self {
            range: start..start + relr_bitmap_slots::<C>() * C::RELR_ENTRY_SIZE,
            encoded: 1,
            class: PhantomData,
        }
    }

    // Return bitmap that will follow after the current bitmap range.
    pub(crate) fn next(&self) -> Self {
        let address_range = relr_bitmap_slots::<C>() * C::RELR_ENTRY_SIZE;
        Self {
            range: self.range.start + address_range..self.range.end + address_range,
            encoded: 1,
            class: PhantomData,
        }
    }

    // Encoding address if properly aligned and fits in the current range. If fits, true is
    // returned.
    pub(crate) fn insert(&mut self, address: u64) -> bool {
        let offset = address.wrapping_sub(self.range.start);
        if !self.range.contains(&address) || !offset.is_multiple_of(C::RELR_ENTRY_SIZE) {
            false
        } else {
            self.encoded |= 1 << (offset / C::RELR_ENTRY_SIZE + 1);
            true
        }
    }
}

/// Tracks RELR bitmap packing within one input section. Runs deliberately don't
/// cross section boundaries because layout of separate sections is parallel.
#[derive(Default)]
pub(crate) enum RelrState<C: ElfClass> {
    #[default]
    NoRun,
    AddressOnly {
        next_bitmap: RelrBitmap<C>,
    },
    WithBitmap {
        bitmap: RelrBitmap<C>,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum RelrEntryEncoding {
    New,
    Update,
}

#[derive(Default)]
pub(crate) struct RelrEncoder<C: ElfClass> {
    pub(crate) state: RelrState<C>,
}

// RELR bitmap packing state used for both allocation and the actual writing of relocations
// to the output stream.
impl<C: ElfClass> RelrEncoder<C> {
    pub(crate) fn encode(
        &mut self,
        address: u64,
        mut encode_fn: impl FnMut(u64, RelrEntryEncoding) -> Result,
    ) -> Result {
        self.state = match std::mem::take(&mut self.state) {
            RelrState::NoRun => {
                encode_fn(address, RelrEntryEncoding::New)?;
                RelrState::AddressOnly {
                    next_bitmap: RelrBitmap::after(address),
                }
            }
            RelrState::AddressOnly { mut next_bitmap } => {
                if next_bitmap.insert(address) {
                    encode_fn(next_bitmap.encoded, RelrEntryEncoding::New)?;
                    RelrState::WithBitmap {
                        bitmap: next_bitmap,
                    }
                } else {
                    encode_fn(address, RelrEntryEncoding::New)?;
                    RelrState::AddressOnly {
                        next_bitmap: RelrBitmap::after(address),
                    }
                }
            }
            RelrState::WithBitmap { mut bitmap } => {
                if bitmap.insert(address) {
                    encode_fn(bitmap.encoded, RelrEntryEncoding::Update)?;
                    RelrState::WithBitmap { bitmap }
                } else {
                    // Current window has bits — try next window.
                    // lld only advances to a new bitmap if the current one is
                    // non-empty (breaks on empty bitmap). Same rule here.
                    let mut next_bitmap = bitmap.next();
                    if next_bitmap.insert(address) {
                        encode_fn(next_bitmap.encoded, RelrEntryEncoding::New)?;
                        RelrState::WithBitmap {
                            bitmap: next_bitmap,
                        }
                    } else {
                        // Gap too large — start new address entry.
                        encode_fn(address, RelrEntryEncoding::New)?;
                        RelrState::AddressOnly {
                            next_bitmap: RelrBitmap::after(address),
                        }
                    }
                }
            }
        };
        Ok(())
    }
}
