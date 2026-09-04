use crate::hash::PreHashed;
use crate::hash::hash_bytes;
use std::fmt::Display;
use std::ops::BitXor as _;

/// A prehashed symbol that may or may not be versioned. Note, we have the enum as the outer layer
/// and prehash inside the enum. It might be tempting to think that we should do this the other way
/// around. i.e. define a type SymbolName, that's either an enum or has an optional version, then
/// prehash that. However, doing that would mean that the type stored in our names map would be
/// larger which would hurt performance. Benchmarks showed about a 2.4% slowdown just from adding an
/// optional version to the type stored in our names map. So instead, we handle versioned and
/// unversioned symbols separately.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PreHashedSymbolName<'data> {
    Unversioned(PreHashed<UnversionedSymbolName<'data>>),
    Versioned(PreHashed<VersionedSymbolName<'data>>),
}

#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnversionedSymbolName<'data> {
    #[debug("{}", String::from_utf8_lossy(bytes))]
    bytes: &'data [u8],
}

#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionedSymbolName<'data> {
    name: UnversionedSymbolName<'data>,
    #[debug("{}", String::from_utf8_lossy(version))]
    version: &'data [u8],
}

impl<'data> UnversionedSymbolName<'data> {
    pub fn new(bytes: &'data [u8]) -> UnversionedSymbolName<'data> {
        Self { bytes }
    }

    pub fn prehashed(bytes: &'data [u8]) -> PreHashed<UnversionedSymbolName<'data>> {
        PreHashed::new(Self::new(bytes), hash_bytes(bytes))
    }

    pub fn bytes(&self) -> &'data [u8] {
        self.bytes
    }
}

impl<'data> VersionedSymbolName<'data> {
    pub fn prehashed(
        name: PreHashed<UnversionedSymbolName<'data>>,
        version: &'data [u8],
    ) -> PreHashed<VersionedSymbolName<'data>> {
        PreHashed::new(
            VersionedSymbolName {
                name: *name,
                version,
            },
            name.hash().bitxor(hash_bytes(version)),
        )
    }
}

impl Display for UnversionedSymbolName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Ok(s) = std::str::from_utf8(self.bytes) {
            Display::fmt(s, f)
        } else {
            write!(f, "INVALID UTF-8({:?})", self.bytes)
        }
    }
}

impl<'data> PreHashedSymbolName<'data> {
    pub fn from_parts(
        name: &'data [u8],
        version: Option<&'data [u8]>,
    ) -> PreHashedSymbolName<'data> {
        let name = UnversionedSymbolName::prehashed(name);
        if let Some(version) = version {
            PreHashedSymbolName::Versioned(VersionedSymbolName::prehashed(name, version))
        } else {
            PreHashedSymbolName::Unversioned(name)
        }
    }
}
