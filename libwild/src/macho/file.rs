use super::MachO;
#[allow(unused_imports)]
use super::abi::*;
#[allow(unused_imports)]
use super::output::*;
#[allow(unused_imports)]
use super::types::*;
use crate::args::macho::MachOArgs;
use crate::ensure;
use crate::error;
use crate::error::Result;
use crate::file_kind::FileKind;
use crate::file_writer::copy_section_data;
use crate::layout;
use crate::platform;
use object::macho;
use object::macho::N_SECT;
use object::read::macho::MachHeader;
use object::read::macho::Nlist;
use object::read::macho::Section;
use object::read::macho::Segment;
use std::borrow::Cow;
use std::slice::Iter;

#[derive(derive_more::Debug)]
pub(crate) struct File<'data> {
    #[debug(skip)]
    pub(crate) data: &'data [u8],
    #[debug(skip)]
    pub(crate) symbols: SymbolTable<'data>,
    #[allow(unused)]
    pub(crate) flags: object::macho::FileFlags,
    pub(super) kind: ObjectKind<'data>,
}

#[derive(Debug)]
pub(super) enum ObjectKind<'data> {
    Regular(RegularObject<'data>),
    Dylib,
}

#[derive(derive_more::Debug)]
pub(super) struct RegularObject<'data> {
    #[debug(skip)]
    pub(crate) sections: SectionTable<'data>,
}

impl<'data> platform::ObjectFile<'data> for File<'data> {
    type Platform = MachO;

    fn parse_bytes(input: &'data [u8], is_dynamic: bool) -> Result<Self> {
        let header = macho::MachHeader64::<object::Endianness>::parse(input, 0)?;
        let mut commands = header.load_commands(LE, input, 0)?;

        let mut symbols = None;
        let mut sections = None;

        while let Some(command) = commands.next()? {
            if let Some(symtab_command) = command.symtab()? {
                ensure!(symbols.is_none(), "At most one symtab command expected");
                symbols = Some(symtab_command.symbols::<macho::MachHeader64<_>, _>(LE, input)?);
            } else if !is_dynamic
                && let Some((segment_command, segment_data)) = command.segment_64()?
            {
                ensure!(sections.is_none(), "At most one segment command expected");
                let section_list = segment_command.sections(LE, segment_data)?;
                sections = Some(section_list);
            }
        }

        let kind = if is_dynamic {
            ObjectKind::Dylib
        } else {
            ObjectKind::Regular(RegularObject {
                sections: sections.ok_or("Missing segment command")?,
            })
        };

        Ok(File {
            data: input,
            symbols: symbols.ok_or("Missing symbol table")?,
            flags: header.flags(LE),
            kind,
        })
    }

    fn parse(input: &crate::input_data::InputBytes<'data>, _args: &MachOArgs) -> Result<Self> {
        // TODO
        Self::parse_bytes(input.data, input.kind == FileKind::MachODylib)
    }

    fn is_dynamic(&self) -> bool {
        matches!(self.kind, ObjectKind::Dylib)
    }

    fn num_symbols(&self) -> usize {
        self.symbols.len()
    }

    fn symbols_iter(&self) -> impl Iterator<Item = &SymtabEntry> {
        self.symbols.iter()
    }

    fn symbol(&self, index: object::SymbolIndex) -> Result<&SymtabEntry> {
        Ok(self.symbols.symbol(index)?)
    }

    fn section_size(&self, header: &SectionHeader) -> Result<u64> {
        Ok(header.size.get(LE))
    }

    fn symbol_name(&self, symbol: &SymtabEntry) -> Result<&'data [u8]> {
        Ok(symbol.name(LE, self.symbols.strings())?)
    }

    fn symbol_offset_in_section(
        &self,
        symbol: &SymtabEntry,
        section_index: object::SectionIndex,
    ) -> Result<u64> {
        let section = self.section(section_index)?;
        // On Mach-O the symbol value is the global offset, not a relative to the start of a
        // section.
        symbol
            .n_value
            .get(LE)
            .checked_sub(section.addr.get(LE))
            .ok_or_else(|| error!("Mach-O symbol value is before its section address"))
    }

    fn num_sections(&self) -> usize {
        self.sections().len()
    }

    fn section_iter<'a>(&'a self) -> Iter<'a, SectionHeader> {
        self.sections().iter()
    }

    fn enumerate_sections(&self) -> impl Iterator<Item = (object::SectionIndex, &SectionHeader)> {
        self.sections()
            .iter()
            .enumerate()
            .map(|(i, section)| (object::SectionIndex(i), section))
    }

    fn section(&self, index: object::SectionIndex) -> Result<&SectionHeader> {
        self.sections()
            .get(index.0)
            .ok_or(error!("section index out of range"))
    }

    fn section_by_name(&self, _name: &str) -> Option<(object::SectionIndex, &SectionHeader)> {
        todo!()
    }

    fn symbol_section(
        &self,
        symbol: &SymtabEntry,
        _index: object::SymbolIndex,
    ) -> Result<Option<object::SectionIndex>> {
        if symbol.n_type.typ() == N_SECT && symbol.n_sect != 0 {
            // The index is one-based, NO_SECT == 0, marks a missing section for the symbol.
            Ok(Some(object::SectionIndex(usize::from(symbol.n_sect - 1))))
        } else {
            Ok(None)
        }
    }

    fn symbol_versions(&self) -> &[()] {
        todo!()
    }

    fn dynamic_symbol_used(
        &self,
        symbol_index: object::SymbolIndex,
        file: &mut layout::DynamicLayoutState<'data, MachO>,
    ) -> Result {
        file.format_specific
            .imported_symbols
            .push(file.symbol_id_range.input_to_id(symbol_index));
        Ok(())
    }

    fn finalise_sizes_dynamic(
        &self,
        _lib_name: &[u8],
        _state: &mut DynamicLayoutStateExt,
        _mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) -> Result {
        Ok(())
    }

    fn apply_non_addressable_indexes_dynamic(
        &self,
        _indexes: &mut NonAddressableIndexes,
        _counts: &mut (),
        _state: &mut DynamicLayoutStateExt,
    ) -> Result {
        Ok(())
    }

    fn section_name(&self, index: object::SectionIndex) -> Result<&'data [u8]> {
        let section = self
            .sections()
            .get(index.0)
            .ok_or(error!("section index out of range"))?;
        Ok(section.name())
    }

    fn raw_section_data(&self, _section: &SectionHeader) -> Result<&'data [u8]> {
        todo!()
    }

    fn section_data(
        &self,
        _section: &SectionHeader,
        _member: &bumpalo_herd::Member<'data>,
        _loaded_metrics: &crate::resolution::LoadedMetrics,
    ) -> Result<&'data [u8]> {
        todo!()
    }

    fn copy_section_data(&self, section: &SectionHeader, out: &mut [u8]) -> Result {
        let data = section
            .data(LE, self.data, section.offset(LE).into())
            .map_err(|_e| error!("cannot get section data"))?;
        copy_section_data(data, out);

        Ok(())
    }

    fn section_data_cow(&self, _section: &SectionHeader) -> Result<std::borrow::Cow<'data, [u8]>> {
        todo!()
    }

    fn section_alignment(&self, section: &SectionHeader) -> Result<u64> {
        Ok(2u64.pow(section.align(LE)))
    }

    fn relocations(
        &self,
        index: object::SectionIndex,
        _relocations: &(),
    ) -> Result<RelocationList<'data>> {
        Ok(RelocationList {
            relocations: self
                .sections()
                .get(index.0)
                .ok_or(error!("section index out of range"))?
                .relocations(LE, self.data)?,
        })
    }

    fn parse_relocations(&self) -> Result<()> {
        Ok(())
    }

    fn symbol_version_debug(&self, _symbol_index: object::SymbolIndex) -> Option<String> {
        None
    }

    fn section_display_name(&self, index: object::SectionIndex) -> Cow<'data, str> {
        self.section_name(index).map_or_else(
            |_| format!("<index {}>", index.0).into(),
            String::from_utf8_lossy,
        )
    }

    fn dynamic_tag_values(&self) -> Option<DynamicTagValues<'data>> {
        match self.kind {
            ObjectKind::Regular(_) => None,
            ObjectKind::Dylib => Some(DynamicTagValues::default()),
        }
    }

    fn get_version_names(&self) -> Result<()> {
        Ok(())
    }

    fn get_symbol_name_and_version(
        &self,
        symbol: &SymtabEntry,
        _local_index: usize,
        _version_names: &(),
    ) -> Result<RawSymbolName<'data>> {
        Ok(RawSymbolName {
            name: self.symbol_name(symbol)?,
        })
    }

    fn should_enforce_undefined(
        &self,
        _resources: &crate::layout::GraphResources<'data, '_, Self::Platform>,
    ) -> bool {
        todo!()
    }

    fn verneed_table(&self) -> Result<VerneedTable<'data>> {
        Ok(VerneedTable { _phantom: &[] })
    }

    fn process_gnu_note_section(
        &self,
        _state: &mut (),
        _section_index: object::SectionIndex,
    ) -> Result {
        todo!()
    }

    fn dynamic_tags(&self) -> Result<&'data [()]> {
        todo!()
    }
}

impl<'data> File<'data> {
    pub(super) fn sections(&self) -> &'data [SectionHeader] {
        self.kind.sections()
    }
}

impl<'data> ObjectKind<'data> {
    pub(super) fn sections(&self) -> &'data [SectionHeader] {
        match self {
            ObjectKind::Regular(regular_object) => regular_object.sections,
            ObjectKind::Dylib => &[],
        }
    }
}
