//! Support for version scripts. Version scripts are used for attaching versions to symbols when
//! producing a shared object and for controlling which symbols do and don't get exported. Version
//! scripts are technically part of the linker script syntax, via the VERSION command, but are
//! generally passed via the `--version-script` flag instead. They can also sometimes be quite
//! large. For this reason, we have a separate parser for them.

pub mod parse;
pub mod types;

#[allow(unused_imports)]
pub use parse::*;
#[allow(unused_imports)]
pub use types::*;
