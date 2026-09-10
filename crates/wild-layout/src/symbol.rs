use crate::platform::RawSymbolName;
pub use wild_util::symbol_name::PreHashedSymbolName;
pub use wild_util::symbol_name::UnversionedSymbolName;
pub use wild_util::symbol_name::VersionedSymbolName;

pub fn symbol_name_from_raw<'data>(
    name_info: &impl RawSymbolName<'data>,
) -> PreHashedSymbolName<'data> {
    PreHashedSymbolName::from_parts(name_info.name(), name_info.version_name())
}
