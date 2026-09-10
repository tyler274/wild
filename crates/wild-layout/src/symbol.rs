use crate::platform::RawSymbolName;
pub(crate) use wild_util::symbol_name::PreHashedSymbolName;
pub(crate) use wild_util::symbol_name::UnversionedSymbolName;
pub(crate) use wild_util::symbol_name::VersionedSymbolName;

pub(crate) fn symbol_name_from_raw<'data>(
    name_info: &impl RawSymbolName<'data>,
) -> PreHashedSymbolName<'data> {
    PreHashedSymbolName::from_parts(name_info.name(), name_info.version_name())
}
