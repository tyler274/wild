//! Reads global symbols for each input file and builds a map from symbol names to IDs together with
//! information about where each symbol can be obtained.

pub mod db;
pub mod ids;
pub mod load;
pub mod select;

#[allow(unused_imports)]
pub use db::*;
#[allow(unused_imports)]
pub use ids::*;
#[allow(unused_imports)]
pub use load::*;
#[allow(unused_imports)]
pub use select::*;
