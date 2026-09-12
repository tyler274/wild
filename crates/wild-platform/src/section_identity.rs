use super::Platform;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy)]
pub struct SectionIdentity<'data, P: Platform> {
    name: SectionName<'data>,
    format_specific: P::SectionIdentityExt,
}

impl<'data, P: Platform> SectionIdentity<'data, P> {
    pub const fn new(name: SectionName<'data>, format_specific: P::SectionIdentityExt) -> Self {
        Self {
            name,
            format_specific,
        }
    }

    pub fn section_name(&self) -> SectionName<'data> {
        self.name
    }

    pub fn format_specific(&self) -> P::SectionIdentityExt {
        self.format_specific
    }
}

impl<'data, P: Platform> PartialEq for SectionIdentity<'data, P> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.format_specific == other.format_specific
    }
}

impl<'data, P: Platform> Eq for SectionIdentity<'data, P> {}

impl<'data, P: Platform> Hash for SectionIdentity<'data, P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.format_specific.hash(state);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionName<'data>(pub &'data [u8]);

impl SectionName<'_> {
    pub fn bytes(&self) -> &[u8] {
        self.0
    }
}

impl Debug for SectionName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", String::from_utf8_lossy(self.0)))
    }
}

impl<P: Platform> Display for SectionIdentity<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        P::fmt_section_identity(self.name, &self.format_specific, f)
    }
}

impl Display for SectionName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.0))
    }
}
