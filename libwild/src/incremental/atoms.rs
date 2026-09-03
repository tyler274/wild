//! Generational identity for incremental linking.
//!
//! Dense `SymbolId` / `FileId` values are rebuilt every link and stay as `vec[id]` indexes on the
//! GC/layout hot path. Cross-run identity uses `{index, generation}` handles so a reused slot
//! cannot alias a deleted file's reverse-reloc lists or resolutions.

use crate::error::Result;
use hashbrown::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;

/// `{index, generation}` handle. Generation 0 is never issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AtomId {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

#[derive(Clone, Debug)]
struct AtomSlot {
    generation: u32,
    /// `None` means the slot is free. Generation is still stored so the next alloc is distinct.
    key: Option<String>,
    num_symbols: u32,
}

/// Generational arena of input files (and other symbol-bearing units) that persist across
/// `--incremental` updates.
#[derive(Clone, Debug, Default)]
pub(crate) struct AtomTable {
    slots: Vec<AtomSlot>,
    free: Vec<u32>,
    by_key: HashMap<String, u32>,
}

impl AtomTable {
    pub(crate) fn alloc(&mut self, key: String, num_symbols: u32) -> AtomId {
        if let Some(&index) = self.by_key.get(&key)
            && let Some(id) = self.live_id(index)
        {
            self.slots[index as usize].num_symbols = num_symbols;
            return id;
        }
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.key = Some(key.clone());
            slot.num_symbols = num_symbols;
            self.by_key.insert(key, index);
            AtomId {
                index,
                generation: slot.generation,
            }
        } else {
            let index = u32::try_from(self.slots.len()).expect("atom table overflowed 32 bits");
            self.slots.push(AtomSlot {
                generation: 1,
                key: Some(key.clone()),
                num_symbols,
            });
            self.by_key.insert(key, index);
            AtomId {
                index,
                generation: 1,
            }
        }
    }

    pub(crate) fn get(&self, id: AtomId) -> bool {
        self.live_id(id.index)
            .is_some_and(|live| live.generation == id.generation)
    }

    pub(crate) fn get_by_key(&self, key: &str) -> Option<AtomId> {
        let index = *self.by_key.get(key)?;
        self.live_id(index)
    }

    pub(crate) fn num_symbols(&self, id: AtomId) -> Option<u32> {
        if !self.get(id) {
            return None;
        }
        Some(self.slots[id.index as usize].num_symbols)
    }

    pub(crate) fn key(&self, id: AtomId) -> Option<&str> {
        if !self.get(id) {
            return None;
        }
        self.slots[id.index as usize].key.as_deref()
    }

    pub(crate) fn free(&mut self, id: AtomId) {
        if !self.get(id) {
            return;
        }
        let slot = &mut self.slots[id.index as usize];
        if let Some(key) = slot.key.take() {
            self.by_key.remove(&key);
        }
        slot.num_symbols = 0;
        slot.generation = next_generation(slot.generation);
        self.free.push(id.index);
    }

    pub(crate) fn live_ids(&self) -> impl Iterator<Item = AtomId> + '_ {
        self.slots.iter().enumerate().filter_map(|(i, slot)| {
            slot.key.as_ref().map(|_| AtomId {
                index: i as u32,
                generation: slot.generation,
            })
        })
    }

    fn live_id(&self, index: u32) -> Option<AtomId> {
        let slot = self.slots.get(index as usize)?;
        if slot.key.is_some() {
            Some(AtomId {
                index,
                generation: slot.generation,
            })
        } else {
            None
        }
    }

    pub(crate) fn write(&self, path: &Path) -> Result {
        let mut out = fs::File::create(path)?;
        for (i, slot) in self.slots.iter().enumerate() {
            match &slot.key {
                Some(key) => writeln!(out, "{} {} {} {key}", i, slot.generation, slot.num_symbols)?,
                None => writeln!(out, "{} {} {}", i, slot.generation, slot.num_symbols)?,
            }
        }
        Ok(())
    }

    pub(crate) fn read(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let mut table = Self::default();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let mut parts = line.splitn(4, ' ');
            let index: usize = parts.next()?.parse().ok()?;
            let generation: u32 = parts.next()?.parse().ok()?;
            if generation == 0 {
                return None;
            }
            let num_symbols: u32 = parts.next()?.parse().ok()?;
            let key = parts.next().filter(|s| !s.is_empty()).map(str::to_owned);
            while table.slots.len() < index {
                table.slots.push(AtomSlot {
                    generation: 1,
                    key: None,
                    num_symbols: 0,
                });
                table.free.push((table.slots.len() - 1) as u32);
            }
            if table.slots.len() == index {
                table.slots.push(AtomSlot {
                    generation,
                    key: key.clone(),
                    num_symbols,
                });
            } else {
                table.slots[index] = AtomSlot {
                    generation,
                    key: key.clone(),
                    num_symbols,
                };
            }
            match key {
                Some(key) => {
                    table.by_key.insert(key, index as u32);
                }
                None => table.free.push(index as u32),
            }
        }
        Some(table)
    }
}

fn next_generation(generation: u32) -> u32 {
    match generation.wrapping_add(1) {
        0 => 1,
        next => next,
    }
}

/// Handle into [`NodeArena`]. `index == u32::MAX` is nil.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NodeId {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl NodeId {
    pub(crate) const NIL: Self = Self {
        index: u32::MAX,
        generation: 0,
    };

    pub(crate) fn is_nil(self) -> bool {
        self.index == u32::MAX
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReverseRelocNode {
    pub(crate) file_offset: u64,
    pub(crate) place: u64,
    pub(crate) addend: i64,
    pub(crate) r_type: u32,
    pub(crate) owner: AtomId,
    pub(crate) next: NodeId,
}

#[derive(Debug)]
struct NodeSlot {
    generation: u32,
    live: bool,
    node: ReverseRelocNode,
}

/// Generational arena for reverse-reloc linked-list nodes.
#[derive(Debug, Default)]
pub(crate) struct NodeArena {
    slots: Vec<NodeSlot>,
    free: Vec<u32>,
}

impl NodeArena {
    fn alloc(&mut self, node: ReverseRelocNode) -> NodeId {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.live = true;
            slot.node = node;
            NodeId {
                index,
                generation: slot.generation,
            }
        } else {
            let index = u32::try_from(self.slots.len()).expect("reloc node arena overflowed");
            self.slots.push(NodeSlot {
                generation: 1,
                live: true,
                node,
            });
            NodeId {
                index,
                generation: 1,
            }
        }
    }

    fn get(&self, id: NodeId) -> Option<&ReverseRelocNode> {
        if id.is_nil() {
            return None;
        }
        let slot = self.slots.get(id.index as usize)?;
        if slot.live && slot.generation == id.generation {
            Some(&slot.node)
        } else {
            None
        }
    }

    fn free(&mut self, id: NodeId) {
        if id.is_nil() {
            return;
        }
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return;
        };
        if !slot.live || slot.generation != id.generation {
            return;
        }
        slot.live = false;
        slot.generation = next_generation(slot.generation);
        self.free.push(id.index);
    }

    #[cfg(test)]
    fn live_count(&self) -> u64 {
        self.slots.iter().filter(|s| s.live).count() as u64
    }
}

#[derive(Clone, Debug)]
struct AtomRelocs {
    generation: u32,
    heads: Vec<NodeId>,
}

/// Reverse-reloc lists keyed by `(atom, local symbol offset)`, not dense `SymbolId`.
#[derive(Debug, Default)]
pub(crate) struct ReverseRelocIndex {
    atoms: Vec<AtomRelocs>,
    nodes: NodeArena,
}

impl ReverseRelocIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(
        &mut self,
        defined: AtomId,
        local_symbol: usize,
        file_offset: u64,
        place: u64,
        addend: i64,
        r_type: u32,
        owner: AtomId,
    ) {
        self.ensure_atom(defined);
        let heads = &mut self.atoms[defined.index as usize].heads;
        if local_symbol >= heads.len() {
            heads.resize(local_symbol + 1, NodeId::NIL);
        }
        let next = heads[local_symbol];
        let id = self.nodes.alloc(ReverseRelocNode {
            file_offset,
            place,
            addend,
            r_type,
            owner,
            next,
        });
        self.atoms[defined.index as usize].heads[local_symbol] = id;
    }

    pub(crate) fn for_each_site(
        &self,
        defined: AtomId,
        local_symbol: usize,
        mut visit: impl FnMut(&ReverseRelocNode),
    ) {
        let Some(atom) = self.atoms.get(defined.index as usize) else {
            return;
        };
        if atom.generation != defined.generation {
            return;
        }
        let Some(&mut_head) = atom.heads.get(local_symbol) else {
            return;
        };
        let mut id = mut_head;
        while let Some(node) = self.nodes.get(id) {
            visit(node);
            id = node.next;
        }
    }

    #[cfg(test)]
    pub(crate) fn live_node_count(&self) -> u64 {
        self.nodes.live_count()
    }

    fn ensure_atom(&mut self, id: AtomId) {
        if self.atoms.len() <= id.index as usize {
            self.atoms
                .resize_with(id.index as usize + 1, || AtomRelocs {
                    generation: 0,
                    heads: Vec::new(),
                });
        }
        if self.atoms[id.index as usize].generation != id.generation {
            let old_heads = std::mem::take(&mut self.atoms[id.index as usize].heads);
            for head in old_heads {
                self.free_chain(head);
            }
            self.atoms[id.index as usize].generation = id.generation;
        }
    }

    fn free_chain(&mut self, mut id: NodeId) {
        while let Some(next) = self.nodes.get(id).map(|node| node.next) {
            self.nodes.free(id);
            id = next;
        }
    }
}

const REVERSE_RELOC_MAGIC: &[u8; 4] = b"WREV";
const REVERSE_RELOC_VERSION: u32 = 2;

pub(crate) fn write_reverse_relocs(path: &Path, index: &ReverseRelocIndex) -> Result {
    // Compact live nodes so the on-disk form is a dense list with generation 1.
    let (compact_atoms, compact_nodes) = compact_reverse_relocs(index);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(REVERSE_RELOC_MAGIC);
    bytes.extend_from_slice(&REVERSE_RELOC_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(compact_atoms.len() as u64).to_le_bytes());
    for atom in &compact_atoms {
        bytes.extend_from_slice(&atom.generation.to_le_bytes());
        bytes.extend_from_slice(&(atom.heads.len() as u32).to_le_bytes());
        for head in &atom.heads {
            bytes.extend_from_slice(&head.index.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&(compact_nodes.len() as u64).to_le_bytes());
    for node in &compact_nodes {
        bytes.extend_from_slice(&node.file_offset.to_le_bytes());
        bytes.extend_from_slice(&node.place.to_le_bytes());
        bytes.extend_from_slice(&node.addend.to_le_bytes());
        bytes.extend_from_slice(&node.r_type.to_le_bytes());
        bytes.extend_from_slice(&node.owner.index.to_le_bytes());
        bytes.extend_from_slice(&node.owner.generation.to_le_bytes());
        bytes.extend_from_slice(&node.next.index.to_le_bytes());
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn compact_reverse_relocs(index: &ReverseRelocIndex) -> (Vec<AtomRelocs>, Vec<ReverseRelocNode>) {
    let mut nodes = Vec::new();
    let mut atoms = Vec::with_capacity(index.atoms.len());
    for src in &index.atoms {
        let mut heads = Vec::with_capacity(src.heads.len());
        for &head in &src.heads {
            heads.push(compact_list(index, head, &mut nodes));
        }
        atoms.push(AtomRelocs {
            generation: src.generation,
            heads,
        });
    }
    (atoms, nodes)
}

fn compact_list(
    index: &ReverseRelocIndex,
    head: NodeId,
    nodes: &mut Vec<ReverseRelocNode>,
) -> NodeId {
    let mut chain = Vec::new();
    let mut id = head;
    while let Some(node) = index.nodes.get(id) {
        chain.push(*node);
        id = node.next;
    }
    if chain.is_empty() {
        return NodeId::NIL;
    }
    let start = nodes.len() as u32;
    let count = chain.len() as u32;
    for (i, mut node) in chain.into_iter().enumerate() {
        let i = i as u32;
        node.next = if i + 1 == count {
            NodeId::NIL
        } else {
            NodeId {
                index: start + i + 1,
                generation: 1,
            }
        };
        nodes.push(node);
    }
    NodeId {
        index: start,
        generation: 1,
    }
}

pub(crate) fn read_reverse_relocs(path: &Path) -> Option<ReverseRelocIndex> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < 16 || bytes.get(..4) != Some(REVERSE_RELOC_MAGIC.as_slice()) {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != REVERSE_RELOC_VERSION {
        return None;
    }
    let mut rest = &bytes[8..];
    let take_u64 = |rest: &mut &[u8]| -> Option<u64> {
        let (head, tail) = rest.split_at_checked(8)?;
        *rest = tail;
        Some(u64::from_le_bytes(head.try_into().ok()?))
    };
    let take_u32 = |rest: &mut &[u8]| -> Option<u32> {
        let (head, tail) = rest.split_at_checked(4)?;
        *rest = tail;
        Some(u32::from_le_bytes(head.try_into().ok()?))
    };
    let take_i64 = |rest: &mut &[u8]| -> Option<i64> {
        let (head, tail) = rest.split_at_checked(8)?;
        *rest = tail;
        Some(i64::from_le_bytes(head.try_into().ok()?))
    };
    let atom_count = take_u64(&mut rest)? as usize;
    let mut atoms = Vec::with_capacity(atom_count);
    for _ in 0..atom_count {
        let generation = take_u32(&mut rest)?;
        let n_heads = take_u32(&mut rest)? as usize;
        let mut heads = Vec::with_capacity(n_heads);
        for _ in 0..n_heads {
            let index = take_u32(&mut rest)?;
            heads.push(if index == u32::MAX {
                NodeId::NIL
            } else {
                NodeId {
                    index,
                    generation: 1,
                }
            });
        }
        atoms.push(AtomRelocs { generation, heads });
    }
    let node_count = take_u64(&mut rest)? as usize;
    let mut slots = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let file_offset = take_u64(&mut rest)?;
        let place = take_u64(&mut rest)?;
        let addend = take_i64(&mut rest)?;
        let r_type = take_u32(&mut rest)?;
        let owner_index = take_u32(&mut rest)?;
        let owner_generation = take_u32(&mut rest)?;
        let next_index = take_u32(&mut rest)?;
        slots.push(NodeSlot {
            generation: 1,
            live: true,
            node: ReverseRelocNode {
                file_offset,
                place,
                addend,
                r_type,
                owner: AtomId {
                    index: owner_index,
                    generation: owner_generation,
                },
                next: if next_index == u32::MAX {
                    NodeId::NIL
                } else {
                    NodeId {
                        index: next_index,
                        generation: 1,
                    }
                },
            },
        });
    }
    Some(ReverseRelocIndex {
        atoms,
        nodes: NodeArena {
            slots,
            free: Vec::new(),
        },
    })
}

const RESOLUTIONS_MAGIC: &[u8; 4] = b"WRES";
const RESOLUTIONS_VERSION: u32 = 1;

#[derive(Clone, Debug, Default)]
pub(crate) struct AtomResolutions {
    slots: Vec<AtomResolutionSlot>,
}

#[derive(Clone, Debug)]
struct AtomResolutionSlot {
    generation: u32,
    values: Vec<u64>,
}

impl AtomResolutions {
    pub(crate) fn set(&mut self, id: AtomId, values: Vec<u64>) {
        if self.slots.len() <= id.index as usize {
            self.slots
                .resize_with(id.index as usize + 1, || AtomResolutionSlot {
                    generation: 0,
                    values: Vec::new(),
                });
        }
        self.slots[id.index as usize] = AtomResolutionSlot {
            generation: id.generation,
            values,
        };
    }

    pub(crate) fn get(&self, id: AtomId) -> Option<&[u64]> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation == id.generation {
            Some(slot.values.as_slice())
        } else {
            None
        }
    }

    pub(crate) fn write(&self, path: &Path) -> Result {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(RESOLUTIONS_MAGIC);
        bytes.extend_from_slice(&RESOLUTIONS_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(self.slots.len() as u64).to_le_bytes());
        for slot in &self.slots {
            bytes.extend_from_slice(&slot.generation.to_le_bytes());
            bytes.extend_from_slice(&(slot.values.len() as u32).to_le_bytes());
            for value in &slot.values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        fs::write(path, bytes)?;
        Ok(())
    }

    pub(crate) fn read(path: &Path) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        if bytes.len() < 16 || bytes.get(..4) != Some(RESOLUTIONS_MAGIC.as_slice()) {
            return None;
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        if version != RESOLUTIONS_VERSION {
            return None;
        }
        let mut rest = &bytes[8..];
        let take_u64 = |rest: &mut &[u8]| -> Option<u64> {
            let (head, tail) = rest.split_at_checked(8)?;
            *rest = tail;
            Some(u64::from_le_bytes(head.try_into().ok()?))
        };
        let take_u32 = |rest: &mut &[u8]| -> Option<u32> {
            let (head, tail) = rest.split_at_checked(4)?;
            *rest = tail;
            Some(u32::from_le_bytes(head.try_into().ok()?))
        };
        let n = take_u64(&mut rest)? as usize;
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            let generation = take_u32(&mut rest)?;
            let len = take_u32(&mut rest)? as usize;
            let mut values = Vec::with_capacity(len);
            for _ in 0..len {
                values.push(take_u64(&mut rest)?);
            }
            slots.push(AtomResolutionSlot { generation, values });
        }
        Some(Self { slots })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_free_invalidates_stale_handle() {
        let mut table = AtomTable::default();
        let a = table.alloc("foo.o".into(), 4);
        assert!(table.get(a));
        table.free(a);
        assert!(!table.get(a));
        let b = table.alloc("bar.o".into(), 2);
        assert_eq!(b.index, a.index);
        assert_ne!(b.generation, a.generation);
        assert!(table.get(b));
        assert!(!table.get(a));
        assert_eq!(table.get_by_key("foo.o"), None);
        assert_eq!(table.get_by_key("bar.o"), Some(b));
    }

    #[test]
    fn alloc_same_key_reuses_live_atom() {
        let mut table = AtomTable::default();
        let a = table.alloc("foo.o".into(), 4);
        let b = table.alloc("foo.o".into(), 4);
        assert_eq!(a, b);
        assert_eq!(table.live_ids().count(), 1);
    }

    #[test]
    fn atom_table_roundtrip_preserves_free_generation() {
        let mut table = AtomTable::default();
        let a = table.alloc("foo.o".into(), 3);
        table.free(a);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atoms.txt");
        table.write(&path).unwrap();
        let loaded = AtomTable::read(&path).unwrap();
        let b = loaded.get_by_key("foo.o");
        assert!(b.is_none());
        let mut loaded = loaded;
        let c = loaded.alloc("bar.o".into(), 1);
        assert_eq!(c.index, a.index);
        assert_ne!(c.generation, a.generation);
        assert!(!loaded.get(a));
    }

    #[test]
    fn reverse_reloc_chains_and_ignores_stale_atom() {
        let mut table = AtomTable::default();
        let def = table.alloc("def.o".into(), 2);
        let owner = table.alloc("use.o".into(), 1);
        let mut index = ReverseRelocIndex::new();
        index.push(def, 1, 0x100, 0x1000, 0, 1, owner);
        index.push(def, 1, 0x200, 0x2000, 0, 1, owner);
        let mut sites = Vec::new();
        index.for_each_site(def, 1, |n| sites.push(n.file_offset));
        assert_eq!(sites, vec![0x200, 0x100]);
        let stale = AtomId {
            index: def.index,
            generation: def.generation.wrapping_add(1),
        };
        let mut stale_sites = Vec::new();
        index.for_each_site(stale, 1, |n| stale_sites.push(n.file_offset));
        assert!(stale_sites.is_empty());
    }

    #[test]
    fn node_arena_reuses_slots_without_aliasing() {
        let mut arena = NodeArena::default();
        let owner = AtomId {
            index: 0,
            generation: 1,
        };
        let id = arena.alloc(ReverseRelocNode {
            file_offset: 1,
            place: 2,
            addend: 0,
            r_type: 1,
            owner,
            next: NodeId::NIL,
        });
        arena.free(id);
        assert!(arena.get(id).is_none());
        let id2 = arena.alloc(ReverseRelocNode {
            file_offset: 9,
            place: 8,
            addend: -1,
            r_type: 2,
            owner,
            next: NodeId::NIL,
        });
        assert_eq!(id2.index, id.index);
        assert_ne!(id2.generation, id.generation);
        assert_eq!(arena.get(id2).unwrap().file_offset, 9);
        assert!(arena.get(id).is_none());
    }

    #[test]
    fn reverse_reloc_roundtrip() {
        let def = AtomId {
            index: 0,
            generation: 1,
        };
        let owner = AtomId {
            index: 1,
            generation: 2,
        };
        let mut index = ReverseRelocIndex::new();
        index.push(def, 0, 0x10, 0x1000, -4, 2, owner);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reverse_relocs.bin");
        write_reverse_relocs(&path, &index).unwrap();
        let loaded = read_reverse_relocs(&path).unwrap();
        let mut sites = Vec::new();
        loaded.for_each_site(def, 0, |n| {
            sites.push((n.file_offset, n.place, n.addend, n.r_type, n.owner));
        });
        assert_eq!(sites, vec![(0x10, 0x1000, -4, 2, owner)]);
        assert_eq!(loaded.live_node_count(), 1);
    }

    #[test]
    fn resolutions_roundtrip_and_generation() {
        let id = AtomId {
            index: 1,
            generation: 3,
        };
        let mut res = AtomResolutions::default();
        res.set(id, vec![10, 20]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolutions.bin");
        res.write(&path).unwrap();
        let loaded = AtomResolutions::read(&path).unwrap();
        assert_eq!(loaded.get(id), Some(&[10, 20][..]));
        assert!(
            loaded
                .get(AtomId {
                    index: 1,
                    generation: 2
                })
                .is_none()
        );
    }
}
