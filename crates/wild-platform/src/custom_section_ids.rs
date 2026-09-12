use super::output_section_id::OutputSectionId;
use super::{Platform, SectionAttributes as _};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrphanClass {
    Exec,
    Ro,
    Data,
    Bss,
    Tdata,
    Tbss,
    NonAlloc,
}

#[derive(Default, Clone)]
pub struct CustomSectionIds {
    pub ro: Vec<OutputSectionId>,
    pub exec: Vec<OutputSectionId>,
    pub data: Vec<OutputSectionId>,
    pub bss: Vec<OutputSectionId>,
    pub nonalloc: Vec<OutputSectionId>,
    pub tdata: Vec<OutputSectionId>,
    pub tbss: Vec<OutputSectionId>,
    /// When a replacing linker script is present, place unnamed (orphan) output
    /// sections after the last section with the same flags, matching GNU ld.
    /// `INSERT` fragments splice into the default layout and do not set this.
    pub place_after_similar: bool,
    /// Script-mentioned custom sections emitted immediately after the previous
    /// builtin named in `SECTIONS`. Without this, those sections are grouped
    /// with orphans (e.g. RO customs before `.text`) and GNU `AT>` LMA
    /// continuation never sees them.
    pub script_followers: Vec<(OutputSectionId, OutputSectionId)>,
}

impl CustomSectionIds {
    pub fn class_of<P: Platform>(attr: &P::SectionAttributes) -> OrphanClass {
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

    pub fn take_class(&mut self, class: OrphanClass) -> Vec<OutputSectionId> {
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
