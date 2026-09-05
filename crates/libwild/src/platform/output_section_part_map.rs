use super::part_id::PartId;
use std::collections::BTreeMap;
use std::mem::take;
use std::ops::AddAssign;
use std::ops::Range;

/// A map from each part of each output section to some value. Different sections are split into
/// parts in different ways. Sections that come from input files are split by alignment. Some
/// sections have no splitting and some have splitting that is specific to that particular section.
/// For example the symbol table is split into local then global symbols.
#[derive(Clone, PartialEq, Eq, derive_more::Debug)]
pub(crate) struct OutputSectionPartMap<T> {
    // TODO: We used to store all the generated parts in separate instance variables. When we
    // switched to instead storing them in this Vec, we saw a small drop in performance (about 2%).
    // This may be due to an extra pointer indirection and/or bounds checking. Experiment with
    // storing all our built-in parts in an array.
    #[debug(skip)]
    parts: Vec<T>,

    #[debug(skip)]
    sparse: Option<Box<SparsePartMap<T>>>,
}

#[derive(Clone, PartialEq, Eq, Default)]
struct SparsePartMap<T> {
    contents: BTreeMap<PartId, T>,
}

impl<T: Default> OutputSectionPartMap<T> {
    pub(crate) fn with_dense_size(size: usize) -> Self {
        let mut parts = Vec::new();
        parts.resize_with(size, Default::default);
        Self {
            parts,
            sparse: None,
        }
    }
}

impl<T> Default for OutputSectionPartMap<T> {
    fn default() -> Self {
        Self {
            parts: Vec::new(),
            sparse: None,
        }
    }
}

impl<T> OutputSectionPartMap<T> {
    pub(crate) fn dense_len(&self) -> usize {
        self.parts.len()
    }
}

pub(crate) enum RangeIterator<'a, T> {
    Dense(PartId, &'a [T]),
    Sparse(std::collections::btree_map::Range<'a, PartId, T>),
}

impl<'a, T> Iterator for RangeIterator<'a, T> {
    type Item = (PartId, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            RangeIterator::Dense(part_id, items) => {
                let item = items.split_off_first()?;
                let id = *part_id;
                *part_id = part_id.offset(1);
                Some((id, item))
            }
            RangeIterator::Sparse(range) => range.next().map(|(&id, item)| (id, item)),
        }
    }
}

impl<T: Default> OutputSectionPartMap<T> {
    pub(crate) fn new_empty_like<U: Default>(&self) -> OutputSectionPartMap<U> {
        OutputSectionPartMap::with_dense_size(self.dense_len())
    }

    pub(crate) fn get_mut(&mut self, part_id: PartId) -> &mut T {
        self.parts.get_mut(part_id.as_usize()).unwrap_or_else(|| {
            self.sparse
                .get_or_insert_default()
                .contents
                .entry(part_id)
                .or_default()
        })
    }

    /// Note, range must be either entirely dense or entirely sparse. Itended use-case is to get all
    /// parts for a single section.
    pub(crate) fn in_range(&self, range: Range<PartId>) -> RangeIterator<'_, T> {
        if let Some(values) = self.parts.get(range.start.as_usize()..range.end.as_usize()) {
            RangeIterator::Dense(range.start, values)
        } else if let Some(sparse) = self.sparse.as_ref() {
            RangeIterator::Sparse(sparse.contents.range(range))
        } else {
            RangeIterator::Sparse(Default::default())
        }
    }

    pub(crate) fn values_in_range(&self, range: Range<PartId>) -> impl Iterator<Item = &T> {
        self.in_range(range).map(|(_, v)| v)
    }
}

impl<T: Default + Copy> OutputSectionPartMap<T> {
    pub(crate) fn get(&self, part_id: PartId) -> T {
        self.parts
            .get(part_id.as_usize())
            .copied()
            .unwrap_or_else(|| {
                self.sparse
                    .as_ref()
                    .and_then(|sparse| sparse.contents.get(&part_id))
                    .copied()
                    .unwrap_or_default()
            })
    }
}

impl<T: Default> OutputSectionPartMap<T> {
    pub(crate) fn take(&mut self, part_id: PartId) -> T {
        take(self.get_mut(part_id))
    }
}

impl OutputSectionPartMap<u64> {
    pub(crate) fn increment(&mut self, part_id: PartId, size: u64) {
        *self.get_mut(part_id) += size;
    }

    pub(crate) fn decrement(&mut self, part_id: PartId, size: u64) {
        let v = self.get_mut(part_id);
        debug_assert!(
            *v >= size,
            "decrement underflow for {part_id:?}: {v} < {size}"
        );
        *v -= size;
    }

    /// Increment `self` by `sizes`. Returns the pre-increment values, but only for entries actually
    /// present in `sizes`.
    pub(crate) fn merge_and_return_start_offsets(&mut self, sizes: &Self) -> Self {
        self.mut_with_map(sizes, |offset, size| {
            let start = *offset;
            *offset += *size;
            start
        })
    }
}

impl<T: Default + PartialEq> OutputSectionPartMap<T> {
    /// Iterate through all contained T, producing a new map of U from the values returned by the
    /// callback.
    pub(crate) fn map<U: Default>(
        &self,
        mut cb: impl FnMut(PartId, &T) -> U,
    ) -> OutputSectionPartMap<U> {
        OutputSectionPartMap {
            parts: self
                .parts
                .iter()
                .enumerate()
                .map(|(i, value)| cb(PartId::from_usize(i), value))
                .collect(),
            sparse: self.sparse.as_ref().map(|sparse| {
                Box::new(SparsePartMap {
                    contents: sparse
                        .contents
                        .iter()
                        .map(|(id, value)| (*id, cb(*id, value)))
                        .collect(),
                })
            }),
        }
    }

    /// Zip mutable references to values in `self` with shared references from `other` producing a
    /// new map with the returned values. For custom sections, `other` must be a subset of `self`.
    /// Values not in `other` will not be in the returned map.
    pub(crate) fn mut_with_map<U: Default, V: Default>(
        &mut self,
        other: &OutputSectionPartMap<U>,
        mut cb: impl FnMut(&mut T, &U) -> V,
    ) -> OutputSectionPartMap<V> {
        let parts = self
            .parts
            .iter_mut()
            .zip(other.parts.iter())
            .map(|(t, u)| cb(t, u))
            .collect();

        let Some(other_sparse) = other.sparse.as_ref() else {
            return OutputSectionPartMap {
                parts,
                sparse: None,
            };
        };

        let self_sparse = self.sparse.get_or_insert_with(|| {
            Box::new(SparsePartMap {
                contents: BTreeMap::new(),
            })
        });

        let contents = other_sparse
            .contents
            .iter()
            .map(|(part_id, right_value)| {
                let left_value = self_sparse.contents.entry(*part_id).or_default();
                (*part_id, cb(left_value, right_value))
            })
            .collect();

        OutputSectionPartMap {
            parts,
            sparse: Some(Box::new(SparsePartMap { contents })),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (PartId, &T)> {
        self.parts
            .iter()
            .enumerate()
            .map(|(i, value)| (PartId::from_usize(i), value))
            .chain(
                self.sparse
                    .as_ref()
                    .map(|sparse| sparse.contents.iter())
                    .unwrap_or_default()
                    .map(|(part_id, value)| (*part_id, value)),
            )
    }
}

impl<T: AddAssign + Copy + Default> OutputSectionPartMap<T> {
    pub(crate) fn merge(&mut self, rhs: &Self) {
        for (left, right) in self.parts.iter_mut().zip(rhs.parts.iter()) {
            *left += *right;
        }

        if let Some(rhs_sparse) = rhs.sparse.as_ref() {
            let lhs_sparse = self.sparse.get_or_insert_default();
            for (part_id, right) in &rhs_sparse.contents {
                *lhs_sparse.contents.entry(*part_id).or_default() += *right;
            }
        }
    }
}

impl<'out> OutputSectionPartMap<&'out mut [u8]> {
    pub(crate) fn take_mut(
        &mut self,
        sizes: &OutputSectionPartMap<usize>,
    ) -> OutputSectionPartMap<&'out mut [u8]> {
        self.mut_with_map(sizes, |buffer, size| buffer.split_off_mut(..*size).unwrap())
    }
}
