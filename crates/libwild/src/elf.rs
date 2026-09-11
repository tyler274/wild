use wild_args::elf::ElfArgs;
use wild_error::bail;
use wild_error::error::Result;
use wild_fs::fs::FileSystem;

pub(crate) fn link_for_arch<'data, F: FileSystem>(
    linker: &'data crate::Linker<F>,
    args: &'data ElfArgs,
) -> Result<crate::LinkerOutput<'data>> {
    match args.architecture() {
        wild_util::arch::Architecture::X86_64 => {
            linker.link_for_arch::<wild_elf::Elf64, wild_elf::ElfX86_64>(args)
        }
        wild_util::arch::Architecture::AArch64 => {
            linker.link_for_arch::<wild_elf::Elf64, wild_elf::ElfAArch64>(args)
        }
        wild_util::arch::Architecture::RiscV64 => {
            linker.link_for_arch::<wild_elf::Elf64, wild_elf::ElfRiscV64>(args)
        }
        wild_util::arch::Architecture::LoongArch64 => {
            linker.link_for_arch::<wild_elf::Elf64, wild_elf::ElfLoongArch64>(args)
        }
        wild_util::arch::Architecture::Ppc64 => {
            linker.link_for_arch::<wild_elf::Elf64, wild_elf::ElfPpc64>(args)
        }
        wild_util::arch::Architecture::Unsupported => {
            bail!(
                "No default target architecture known for host platform. \
                    Please specify an architecture with -m"
            )
        }
    }
}
