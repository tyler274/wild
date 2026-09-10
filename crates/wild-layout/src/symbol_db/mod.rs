//! Reads global symbols for each input file and builds a map from symbol names to IDs together with
//! information about where each symbol can be obtained.

pub(crate) mod db;
pub(crate) mod ids;
pub(crate) mod load;
pub(crate) mod select;

#[allow(unused_imports)]
pub(crate) use db::*;
#[allow(unused_imports)]
pub(crate) use ids::*;
#[allow(unused_imports)]
pub(crate) use load::*;
#[allow(unused_imports)]
pub(crate) use select::*;
