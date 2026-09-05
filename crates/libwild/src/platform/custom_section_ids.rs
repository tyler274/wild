use super::Platform;
use super::SectionAttributes as _;
use super::output_section_id::OutputSectionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OrphanClass {
    Exec,
    Ro,
    Data,
    Bss,
    Tdata,
    Tbss,
    NonAlloc,
}

#[derive(Default, Clone)]
pub(crate) struct CustomSectionIds {
    pub(crate) ro: Vec<OutputSectionId>,
    pub(crate) exec: Vec<OutputSectionId>,
    pub(crate) data: Vec<OutputSectionId>,
    pub(crate) bss: Vec<OutputSectionId>,
    pub(crate) nonalloc: Vec<OutputSectionId>,
    pub(crate) tdata: Vec<OutputSectionId>,
    pub(crate) tbss: Vec<OutputSectionId>,
    /// When a linker script is present, place unnamed (orphan) output sections
    /// after the last section with the same flags, matching GNU ld.
    pub(crate) place_after_similar: bool,
}

impl CustomSectionIds {
    pub(crate) fn class_of<P: Platform>(attr: &P::SectionAttributes) -> OrphanClass {
        if attr.is_executable() {
            OrphanClass::Exec
        } else if attr.is_tls() {
            if attr.is_no_bits() {
                OrphanClass::Tbss
            } else {
                OrphanClass::Tdata
            }
        } else if !attr.is_writable() {
            if attr.is_alloc() {
                OrphanClass::Ro
            } else {
                OrphanClass::NonAlloc
            }
        } else if attr.is_no_bits() {
            OrphanClass::Bss
        } else {
            OrphanClass::Data
        }
    }

    pub(crate) fn take_class(&mut self, class: OrphanClass) -> Vec<OutputSectionId> {
        match class {
            OrphanClass::Exec => core::mem::take(&mut self.exec),
            OrphanClass::Ro => core::mem::take(&mut self.ro),
            OrphanClass::Data => core::mem::take(&mut self.data),
            OrphanClass::Bss => core::mem::take(&mut self.bss),
            OrphanClass::Tdata => core::mem::take(&mut self.tdata),
            OrphanClass::Tbss => core::mem::take(&mut self.tbss),
            OrphanClass::NonAlloc => core::mem::take(&mut self.nonalloc),
        }
    }
}
