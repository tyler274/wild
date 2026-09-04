//! ELF `.strtab` / `.shstrtab` builder with GNU ld suffix sharing.
//!
//! BFD `_bfd_elf_strtab_finalize` reverse-sorts unique names and absorbs a string into the
//! neighbouring longer host when it is a suffix, so `"bar"` can point into `"foobar"`. Offset 0
//! is always the empty string.

use crate::error::Result;
use crate::layout::EnginePlatform;
use crate::layout_rules::SectionKind;
use crate::output_section_id::OutputSections;
use hashbrown::HashMap;
use hashbrown::HashSet;

/// Final `.strtab` contents and `st_name` offsets after suffix merging.
#[derive(Debug, Default)]
pub(crate) struct FinalizedStrtab {
    pub(crate) bytes: Vec<u8>,
    offsets: HashMap<Box<[u8]>, u32>,
}

impl FinalizedStrtab {
    pub(crate) fn offset(&self, name: &[u8]) -> Result<u32> {
        if name.is_empty() {
            return Ok(0);
        }
        self.offsets
            .get(name)
            .copied()
            .ok_or_else(|| crate::error!(".strtab is missing `{}`", String::from_utf8_lossy(name)))
    }
}

pub(crate) fn intern_strtab_name(names: &mut Vec<Box<[u8]>>, name: &[u8]) {
    if !name.is_empty() {
        names.push(Box::from(name));
    }
}

pub(crate) fn intern_strtab_name_with_suffix(
    names: &mut Vec<Box<[u8]>>,
    name: &[u8],
    suffix: &[u8],
) {
    let mut bytes = Vec::with_capacity(name.len() + suffix.len());
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(suffix);
    names.push(bytes.into_boxed_slice());
}

/// Reverse-compare like BFD `strrevcmp` with lengths excluding the trailing NUL.
fn strrevcmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    for (ca, cb) in a.iter().rev().zip(b.iter().rev()) {
        if ca != cb {
            return ca.cmp(cb);
        }
    }
    a.len().cmp(&b.len())
}

pub(crate) fn finalize_strtab(names: impl IntoIterator<Item = Box<[u8]>>) -> FinalizedStrtab {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        if name.is_empty() {
            continue;
        }
        if seen.insert(name.clone()) {
            unique.push(name);
        }
    }

    let mut absorbed = vec![None; unique.len()];
    if unique.len() >= 2 {
        let mut order: Vec<usize> = (0..unique.len()).collect();
        order.sort_by(|&i, &j| strrevcmp(&unique[i], &unique[j]));
        let mut host = order[order.len() - 1];
        for k in (0..order.len() - 1).rev() {
            let cmp = order[k];
            if unique[host].len() > unique[cmp].len() && unique[host].ends_with(&unique[cmp]) {
                absorbed[cmp] = Some(host);
            } else {
                host = cmp;
            }
        }
    }

    let mut bytes = vec![0u8];
    let mut offsets = HashMap::new();
    let mut kept_offsets = vec![0u32; unique.len()];
    for (i, name) in unique.iter().enumerate() {
        if absorbed[i].is_some() {
            continue;
        }
        let off = u32::try_from(bytes.len()).expect("Symbol string table overflowed 32 bits");
        kept_offsets[i] = off;
        bytes.extend_from_slice(name);
        bytes.push(0);
        offsets.insert(name.clone(), off);
    }
    for (i, name) in unique.iter().enumerate() {
        let Some(host) = absorbed[i] else {
            continue;
        };
        let off = kept_offsets[host] + (unique[host].len() - name.len()) as u32;
        offsets.insert(name.clone(), off);
    }

    FinalizedStrtab { bytes, offsets }
}

/// Emitted primary section names, suffix-merged like GNU `.shstrtab`.
pub(crate) fn shstrtab_from_sections<'data, P: EnginePlatform>(
    output_sections: &OutputSections<'data, P>,
) -> FinalizedStrtab {
    let mut names = Vec::new();
    for (id, info) in output_sections.ids_with_info() {
        if output_sections.output_index_of_section(id).is_none() {
            continue;
        }
        let SectionKind::Primary(identity) = info.kind else {
            continue;
        };
        intern_strtab_name(&mut names, identity.section_name().bytes());
    }
    let mut tab = finalize_strtab(names);
    if tab.bytes.is_empty() {
        tab.bytes.push(0);
    }
    tab
}

#[cfg(test)]
mod tests {
    use super::finalize_strtab;
    use super::intern_strtab_name;

    fn names(list: &[&[u8]]) -> Vec<Box<[u8]>> {
        let mut out = Vec::new();
        for n in list {
            intern_strtab_name(&mut out, n);
        }
        out
    }

    #[test]
    fn empty_name_is_offset_zero() {
        let tab = finalize_strtab(names(&[b"foo"]));
        assert_eq!(tab.offset(b"").unwrap(), 0);
        assert_eq!(tab.bytes[0], 0);
    }

    #[test]
    fn suffix_shares_storage() {
        let tab = finalize_strtab(names(&[b"bar", b"foobar"]));
        let host = tab.offset(b"foobar").unwrap() as usize;
        let suffix = tab.offset(b"bar").unwrap() as usize;
        assert_eq!(&tab.bytes[host..host + 7], b"foobar\0");
        assert_eq!(suffix, host + "foo".len());
        assert_eq!(&tab.bytes[suffix..suffix + 4], b"bar\0");
        assert_eq!(tab.bytes.iter().filter(|&&b| b == 0).count(), 2);
    }

    #[test]
    fn chained_suffixes_share_the_longest_host() {
        let tab = finalize_strtab(names(&[b"d", b"bcd", b"abcd"]));
        let host = tab.offset(b"abcd").unwrap() as usize;
        assert_eq!(tab.offset(b"bcd").unwrap() as usize, host + 1);
        assert_eq!(tab.offset(b"d").unwrap() as usize, host + 3);
        assert_eq!(tab.bytes.iter().filter(|&&b| b == 0).count(), 2);
    }

    #[test]
    fn exact_duplicates_are_deduped() {
        let tab = finalize_strtab(names(&[b"foo", b"foo"]));
        assert_eq!(tab.offset(b"foo").unwrap(), 1);
        assert_eq!(tab.bytes, b"\0foo\0");
    }

    #[test]
    fn non_suffix_neighbours_stay_separate() {
        let tab = finalize_strtab(names(&[b"abc", b"xbc"]));
        assert_ne!(tab.offset(b"abc").unwrap(), tab.offset(b"xbc").unwrap());
        assert!(tab.bytes.windows(4).any(|w| w == b"abc\0"));
        assert!(tab.bytes.windows(4).any(|w| w == b"xbc\0"));
    }
}
