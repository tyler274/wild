use super::object::{CommonSymbol, SectionFlags, SectionHeader, SectionType, Symbol, Visibility};
use object::macho::{
    N_ABS, N_EXT, N_PEXT, N_WEAK_DEF, S_ATTR_PURE_INSTRUCTIONS, S_ATTR_SOME_INSTRUCTIONS,
    S_GB_ZEROFILL, S_THREAD_LOCAL_ZEROFILL, S_ZEROFILL, Section64,
};
use object::read::macho::{Nlist, Section};
use object::{Endianness, macho};

const LE: Endianness = Endianness::Little;

#[derive(Clone, Copy, PartialEq, Eq)]
struct SegmentName([u8; 16]);

impl SegmentName {
    const PAGEZERO: Self = Self::from_bytes(b"__PAGEZERO");
    const TEXT: Self = Self::from_bytes(b"__TEXT");
    const LINKEDIT: Self = Self::from_bytes(b"__LINKEDIT");
    const LLVM: Self = Self::from_bytes(b"__LLVM");

    const fn from_bytes(name: &[u8]) -> Self {
        assert!(name.len() <= 16);
        let mut bytes = [0; 16];
        bytes.split_at_mut(name.len()).0.copy_from_slice(name);
        Self(bytes)
    }

    fn is_writable(self) -> bool {
        !matches!(self, Self::PAGEZERO | Self::TEXT | Self::LINKEDIT)
    }
}

impl SectionHeader for Section64<Endianness> {
    fn is_alloc(&self) -> bool {
        // TODO: Surely not everything is alloc. But this is for now consistent with
        // SectionFlags::is_alloc.
        true
    }

    fn is_writable(&self) -> bool {
        SegmentName::from_bytes(self.segment_name()).is_writable()
    }

    fn is_executable(&self) -> bool {
        self.flags
            .get(LE)
            .intersects(S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS)
    }

    fn is_tls(&self) -> bool {
        todo!()
    }

    fn is_merge_section(&self) -> bool {
        // TODO
        false
    }

    fn is_strings(&self) -> bool {
        todo!()
    }

    fn should_retain(&self) -> bool {
        // TODO
        false
    }

    fn should_exclude(&self) -> bool {
        // TODO: We need support for sections backed by the Mach-O indirect symbol table for dynamic
        // linking.
        self.flags.get(LE).intersects(macho::S_ATTR_DEBUG)
            || matches!(
                SegmentName::from_bytes(self.segment_name()),
                SegmentName::PAGEZERO | SegmentName::LINKEDIT | SegmentName::LLVM
            )
            || matches!(
                self.flags.get(LE).typ(),
                macho::S_NON_LAZY_SYMBOL_POINTERS
                    | macho::S_LAZY_SYMBOL_POINTERS
                    | macho::S_SYMBOL_STUBS
                    | macho::S_LAZY_DYLIB_SYMBOL_POINTERS
                    | macho::S_THREAD_LOCAL_VARIABLE_POINTERS
            )
    }

    fn is_group(&self) -> bool {
        todo!()
    }

    fn is_note(&self) -> bool {
        false
    }

    fn is_prog_bits(&self) -> bool {
        todo!()
    }

    fn is_no_bits(&self) -> bool {
        matches!(
            self.flags.get(LE).typ(),
            S_ZEROFILL | S_GB_ZEROFILL | S_THREAD_LOCAL_ZEROFILL
        )
    }
}

impl SectionType for macho::SectionType {
    fn is_rela(&self) -> bool {
        todo!()
    }

    fn is_rel(&self) -> bool {
        todo!()
    }

    fn is_symtab(&self) -> bool {
        todo!()
    }

    fn is_strtab(&self) -> bool {
        todo!()
    }
}

impl SectionFlags for macho::SectionFlags {
    fn is_alloc(self) -> bool {
        true
    }
}

impl Symbol for macho::Nlist64<Endianness> {
    fn as_common(&self) -> Option<CommonSymbol> {
        // TODO
        None
    }

    fn is_undefined(&self) -> bool {
        Nlist::is_undefined(self)
    }

    fn is_local(&self) -> bool {
        !self.n_type.contains(N_EXT)
    }

    fn is_absolute(&self) -> bool {
        self.n_type.typ() == N_ABS
    }

    fn is_weak(&self) -> bool {
        self.n_desc.get(LE).contains(N_WEAK_DEF)
    }

    fn visibility(&self) -> Visibility {
        if self.n_type.contains(N_PEXT) {
            Visibility::Hidden
        } else {
            Visibility::Default
        }
    }

    fn value(&self) -> u64 {
        self.n_value.get(LE)
    }

    fn size(&self) -> u64 {
        // TODO
        0
    }

    fn has_name(&self) -> bool {
        self.n_strx.get(LE) != 0
    }

    fn is_default_strippable(&self, name: &[u8]) -> bool {
        Symbol::is_local(self) && name.starts_with(b"ltmp")
    }

    fn debug_string(&self) -> String {
        // TODO
        String::new()
    }

    fn is_tls(&self) -> bool {
        // TODO: derive from section name
        false
    }

    fn is_interposable(&self) -> bool {
        self.visibility() == Visibility::Default
    }

    fn is_func(&self) -> bool {
        // TODO: derive from section name
        false
    }

    fn is_ifunc(&self) -> bool {
        false
    }

    fn is_hidden(&self) -> bool {
        self.visibility() == Visibility::Hidden
    }

    fn is_gnu_unique(&self) -> bool {
        false
    }

    fn with_hidden(mut self, hidden: bool) -> Self {
        if hidden {
            self.n_type.insert(N_PEXT);
        } else {
            self.n_type.remove(N_PEXT);
        }
        self
    }
}
