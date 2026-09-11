use wild_args::wasm::WasmArgs;
use wild_error::bail;
use wild_fs::fs::FileSystem;
use wild_platform::Args as _;

pub(crate) fn link_for_arch<'data, F: FileSystem>(
    linker: &'data crate::Linker<F>,
    args: &'data WasmArgs,
) -> wild_error::error::Result<crate::LinkerOutput<'data>> {
    if !(cfg!(feature = "wasm") || args.experimental_platforms()) {
        bail!("Wasm support is still experimental. Rebuild with `--features wasm` to enable it.");
    }

    linker.link_for_arch::<wild_wasm::Wasm, wild_wasm::WasmWasm32>(args)
}
