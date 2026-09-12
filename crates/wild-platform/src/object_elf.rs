use super::object::{
    CommonSymbol, SectionFlags, SectionHeader, SectionType, SegmentType, Symbol, Visibility,
};
use linker_utils::elf::{shf, sht};
use object::LittleEndian;
use object::elf::{SectionHeader64, Sym64};
use object::read::elf::SectionHeader as _;
use wild_util::alignment::Alignment;

impl SectionHeader for SectionHeader64<LittleEndian> {
    fn is_alloc(&self) -> bool {
        self.sh_flags(LittleEndian).is_alloc()
    }

    fn is_writable(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::WRITE)
    }

    fn is_executable(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::EXECINSTR)
    }

    fn is_tls(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::TLS)
    }

    fn is_merge_section(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::MERGE)
    }

    fn is_strings(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::STRINGS)
    }

    fn merge_entsize(&self) -> u64 {
        self.sh_entsize(LittleEndian).into()
    }

    fn should_retain(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::GNU_RETAIN)
    }

    fn should_exclude(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::EXCLUDE)
    }

    fn elf_sh_flags(&self) -> u64 {
        self.sh_flags(LittleEndian).0
    }

    fn is_group(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::GROUP)
    }

    fn is_note(&self) -> bool {
        self.sh_type(LittleEndian) == sht::NOTE
    }

    fn is_prog_bits(&self) -> bool {
        self.sh_type(LittleEndian) == sht::PROGBITS
    }

    fn is_no_bits(&self) -> bool {
        self.sh_type(LittleEndian) == sht::NOBITS
    }

    fn skip_linker_script_matching(&self) -> bool {
        let ty = self.sh_type(LittleEndian);
        matches!(
            ty,
            sht::REL
                | sht::RELA
                | sht::SYMTAB
                | sht::STRTAB
                | sht::DYNSYM
                | sht::GROUP
                | sht::SYMTAB_SHNDX
        )
    }

    fn is_reloc_section(&self) -> bool {
        matches!(self.sh_type(LittleEndian), sht::REL | sht::RELA)
    }

    fn reloc_output_name_prefix(&self) -> Option<&'static [u8]> {
        match self.sh_type(LittleEndian) {
            sht::RELA => Some(b".rela"),
            sht::REL => Some(b".rel"),
            _ => None,
        }
    }

    fn reloc_target_section_index(&self) -> Option<object::SectionIndex> {
        if !self.is_reloc_section() {
            return None;
        }
        let info = self.sh_info(LittleEndian) as usize;
        (info != 0).then_some(object::SectionIndex(info))
    }
}

impl SectionType for linker_utils::elf::SectionType {
    fn is_rela(&self) -> bool {
        *self == sht::RELA
    }

    fn is_rel(&self) -> bool {
        *self == sht::REL
    }

    fn is_symtab(&self) -> bool {
        *self == sht::SYMTAB
    }

    fn is_strtab(&self) -> bool {
        *self == sht::STRTAB
    }
}

impl SectionFlags for linker_utils::elf::SectionFlags {
    fn is_alloc(self) -> bool {
        self.contains(shf::ALLOC)
    }
}

impl SegmentType for linker_utils::elf::SegmentType {}

impl Symbol for Sym64<LittleEndian> {
    fn as_common(&self) -> Option<CommonSymbol> {
        let e = LittleEndian;
        if !object::read::elf::Sym::is_common(self, e) {
            return None;
        }

        // Common symbols misuse the value field (which we access via `address()`) to store
        // the alignment.
        let Ok(alignment) = Alignment::new(object::read::elf::Sym::st_value(self, e).into()) else {
            return None;
        };
        let size = alignment.align_up(object::read::elf::Sym::st_size(self, e).into());

        Some(CommonSymbol {
            size,
            alignment,
            is_tls: self.st_type() == object::elf::STT_TLS,
        })
    }

    fn is_undefined(&self) -> bool {
        object::read::elf::Sym::is_undefined(self, LittleEndian)
    }

    fn is_local(&self) -> bool {
        object::read::elf::Sym::is_local(self)
    }

    fn visibility(&self) -> Visibility {
        match self.st_visibility() {
            object::elf::STV_PROTECTED => Visibility::Protected,
            object::elf::STV_HIDDEN => Visibility::Hidden,
            _ => Visibility::Default,
        }
    }

    fn is_absolute(&self) -> bool {
        object::read::elf::Sym::is_absolute(self, LittleEndian)
    }

    fn is_weak(&self) -> bool {
        object::read::elf::Sym::is_weak(self)
    }

    fn value(&self) -> u64 {
        object::read::elf::Sym::st_value(self, LittleEndian).into()
    }

    fn size(&self) -> u64 {
        object::read::elf::Sym::st_size(self, LittleEndian).into()
    }

    fn has_name(&self) -> bool {
        object::read::elf::Sym::st_name(self, LittleEndian) != 0
    }

    fn is_default_strippable(&self, name: &[u8]) -> bool {
        (object::read::elf::Sym::is_local(self) && name.starts_with(b".L"))
            || name.starts_with(b"$x")
            || name.starts_with(b"$d")
            || name == b"L0\x01"
    }

    fn debug_string(&self) -> String {
        let e = LittleEndian;
        let vis = if object::read::elf::Sym::is_local(self) {
            "Local"
        } else if object::read::elf::Sym::is_weak(self) {
            "Weak"
        } else {
            "Global"
        };

        let kind = if object::read::elf::Sym::is_undefined(self, e) {
            "Undefined"
        } else {
            match object::read::elf::Sym::st_type(self) {
                object::elf::STT_FUNC => "Func",
                object::elf::STT_GNU_IFUNC => "IFunc",
                object::elf::STT_OBJECT => "Data",
                object::elf::STT_COMMON => "Common",
                object::elf::STT_SECTION => "Section",
                object::elf::STT_FILE => "File",
                object::elf::STT_NOTYPE => "NoType",
                object::elf::STT_TLS => "Tls",
                _ => "Unknown",
            }
        };

        format!("{vis} {kind}")
    }

    fn is_tls(&self) -> bool {
        self.st_type() == object::elf::STT_TLS
    }

    fn is_interposable(&self) -> bool {
        self.st_visibility() == object::elf::STV_DEFAULT
    }

    fn is_func(&self) -> bool {
        self.st_type() == object::elf::STT_FUNC
    }

    fn is_ifunc(&self) -> bool {
        self.st_type() == object::elf::STT_GNU_IFUNC
    }

    fn is_hidden(&self) -> bool {
        self.st_visibility() == object::elf::STV_HIDDEN
    }

    fn is_gnu_unique(&self) -> bool {
        self.st_bind() == object::elf::STB_GNU_UNIQUE
    }

    fn with_hidden(mut self, hidden: bool) -> Self {
        self.st_other = self.st_other.with_visibility(if hidden {
            object::elf::STV_HIDDEN
        } else {
            object::elf::STV_DEFAULT
        });
        self
    }
}
