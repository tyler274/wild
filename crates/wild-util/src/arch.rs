use object::elf::EM_AARCH64;
use object::elf::EM_LOONGARCH;
use object::elf::EM_PPC64;
use object::elf::EM_RISCV;
use object::elf::EM_X86_64;
use std::fmt::Display;
use wild_error::bail;
use wild_error::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    X86_64,
    AArch64,
    RiscV64,
    LoongArch64,
    Ppc64,
    Unsupported,
}

impl TryFrom<object::elf::Machine> for Architecture {
    type Error = wild_error::error::Error;

    fn try_from(arch: object::elf::Machine) -> Result<Self, Self::Error> {
        match arch {
            EM_X86_64 => Ok(Self::X86_64),
            EM_AARCH64 => Ok(Self::AArch64),
            EM_RISCV => Ok(Self::RiscV64),
            EM_LOONGARCH => Ok(Self::LoongArch64),
            EM_PPC64 => Ok(Self::Ppc64),
            _ => bail!("Unsupported architecture: 0x{:x}", arch),
        }
    }
}

impl Display for Architecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let arch = match self {
            Architecture::X86_64 => "x86_64",
            Architecture::AArch64 => "aarch64",
            Architecture::RiscV64 => "riscv64",
            Architecture::LoongArch64 => "loongarch64",
            Architecture::Ppc64 => "ppc64",
            Architecture::Unsupported => "unsupported",
        };
        write!(f, "{arch}")
    }
}

impl Architecture {
    pub fn parse_output_format(format: &[u8]) -> Self {
        let Some(format) = format.strip_prefix(b"elf64-") else {
            return Self::Unsupported;
        };

        match format {
            b"x86-64" => Self::X86_64,
            b"aarch64" | b"littleaarch64" => Self::AArch64,
            b"littleriscv" => Self::RiscV64,
            b"loongarch" => Self::LoongArch64,
            b"powerpcle" => Self::Ppc64,
            _ => Self::Unsupported,
        }
    }

    /// BFD `OUTPUT_ARCH` names used by kernel `vmlinux.lds` and GNU ld.
    pub fn parse_output_arch(arch: &[u8]) -> Self {
        match arch {
            b"i386:x86-64" | b"x86-64" => Self::X86_64,
            b"aarch64" => Self::AArch64,
            b"riscv" => Self::RiscV64,
            b"loongarch" => Self::LoongArch64,
            b"powerpc:common64" => Self::Ppc64,
            _ => Self::Unsupported,
        }
    }
}

pub const SUPPORTED_TARGETS: &str =
    "elf64-x86-64 elf64-littleaarch64 elf64-littleriscv elf64-loongarch elf64-powerpcle";

#[cfg(test)]
mod tests {
    use super::Architecture;

    #[test]
    fn output_arch_kernel_names() {
        assert_eq!(
            Architecture::parse_output_arch(b"i386:x86-64"),
            Architecture::X86_64
        );
        assert_eq!(
            Architecture::parse_output_arch(b"aarch64"),
            Architecture::AArch64
        );
        assert_eq!(
            Architecture::parse_output_arch(b"riscv"),
            Architecture::RiscV64
        );
        assert_eq!(
            Architecture::parse_output_arch(b"loongarch"),
            Architecture::LoongArch64
        );
        assert_eq!(
            Architecture::parse_output_arch(b"powerpc:common64"),
            Architecture::Ppc64
        );
        assert_eq!(
            Architecture::parse_output_arch(b"i386"),
            Architecture::Unsupported
        );
        assert_eq!(
            Architecture::parse_output_arch(b"powerpc:common"),
            Architecture::Unsupported
        );
    }
}
