//! The reference counts of structure starts (`StructureStart.references`,
//! `StructureCheck.incrementReference`) that exploration maps add (`skip_existing_chunks`):
//! a referenced start is skipped by the next map search. Kept per world and saved as the
//! saved data [`SAVED_DATA`]; starts are found by the start chunk and the structure.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use vanillite_base::ChunkPos;
use vanillite_base::hash::FxHashMap;
use vanillite_nbt::{Compound, List, Tag};

/// The saved data id.
pub const SAVED_DATA: &str = "vanillite_structure_references";

/// The references added to structure starts, by start chunk and structure.
#[derive(Debug, Default)]
pub struct StructureReferences {
    added: Mutex<FxHashMap<(ChunkPos, Box<str>), i32>>,
    dirty: AtomicBool,
}

impl StructureReferences {
    fn with<R>(&self, f: impl FnOnce(&mut FxHashMap<(ChunkPos, Box<str>), i32>) -> R) -> R {
        f(&mut self
            .added
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// `getReferences()` of the start of `structure` in `chunk`.
    pub fn references(&self, chunk: ChunkPos, structure: &str) -> i32 {
        self.with(|m| m.get(&(chunk, Box::from(structure))).copied().unwrap_or(0))
    }

    /// `StructureManager.addReference(start)`.
    pub fn add_reference(&self, chunk: ChunkPos, structure: &str) {
        self.with(|m| {
            let n = m.entry((chunk, Box::from(structure))).or_insert(0);
            *n = n.saturating_add(1);
        });
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// `tryAddReference`: references a start that is not referenced yet (`canBeReferenced`).
    pub fn try_add_reference(&self, chunk: ChunkPos, structure: &str) -> bool {
        let free = self.references(chunk, structure) < 1;
        if free {
            self.add_reference(chunk, structure);
        }
        free
    }

    /// Whether something changed since the last call.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// The saved form: `{references: [{x, z, structure, count}, ...]}`.
    pub fn to_nbt(&self) -> Compound {
        let list = self.with(|m| {
            let mut entries: Vec<_> = m
                .iter()
                .map(|((c, s), n)| (c.x, c.z, s.clone(), *n))
                .collect();
            entries.sort();
            entries
                .into_iter()
                .map(|(x, z, s, n)| {
                    Tag::Compound(
                        Compound::new()
                            .with("x", x)
                            .with("z", z)
                            .with("structure", &*s)
                            .with("count", n),
                    )
                })
                .collect()
        });
        Compound::new().with("references", Tag::List(List(list)))
    }

    /// Reads the saved form.
    pub fn load(tag: &Compound) -> StructureReferences {
        let out = StructureReferences::default();
        if let Some(Tag::List(list)) = tag.get("references") {
            out.with(|m| {
                for e in &list.0 {
                    let Tag::Compound(c) = e else {
                        continue;
                    };
                    let int = |k: &str| match c.get(k) {
                        Some(Tag::Int(v)) => Some(*v),
                        _ => None,
                    };
                    if let (Some(x), Some(z), Some(s), Some(n)) = (
                        int("x"),
                        int("z"),
                        c.get("structure").and_then(Tag::as_str),
                        int("count"),
                    ) {
                        m.insert((ChunkPos::new(x, z), Box::from(s)), n);
                    }
                }
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_round_trip() {
        let r = StructureReferences::default();
        let c = ChunkPos::new(-3, 7);
        assert!(r.try_add_reference(c, "minecraft:mansion"));
        assert!(!r.try_add_reference(c, "minecraft:mansion"));
        assert!(r.try_add_reference(c, "minecraft:monument"));
        assert!(r.take_dirty() && !r.take_dirty());
        let back = StructureReferences::load(&r.to_nbt());
        assert_eq!(back.references(c, "minecraft:mansion"), 1);
        assert_eq!(back.references(ChunkPos::new(0, 0), "minecraft:mansion"), 0);
    }
}
