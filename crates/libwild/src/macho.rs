use wild_args::macho::MachOArgs;
use wild_error::error::Result;
use wild_fs::fs::FileSystem;
use wild_platform::Args as _;

pub(crate) fn link_for_arch<'data, F: FileSystem>(
    linker: &'data crate::Linker<F>,
    args: &'data MachOArgs,
) -> Result<crate::LinkerOutput<'data>> {
    if !(cfg!(feature = "macho") || args.experimental_platforms()) {
        wild_error::bail!(
            "Mach-O support is still experimental. Rebuild with `--features macho` to enable it."
        );
    }

    linker.link_for_arch::<wild_macho::MachO, wild_macho::MachOAArch64>(args)
}
