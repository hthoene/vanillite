//! Shared foundations: Java-exact maths, RNGs, positions, AABB, slot map, bitsets, per-tick arena and the hashing policy.
//!
//! Everything here is plain data and pure functions. Nothing knows about worlds, threads, the
//! network or the server.

pub mod aabb;
pub mod arena;
pub mod bitset;
pub mod date;
pub mod fpenv;
pub mod hash;
pub mod math;
mod mth_tables;
pub mod pos;
pub mod prefetch;
pub mod random;
pub mod scratch;
pub mod slot_map;
pub mod vec3;

pub use aabb::Aabb;
pub use arena::TickArena;
pub use bitset::BitSet;
pub use pos::{BlockPos, ChunkPos, Direction, SectionPos};
pub use random::{
    LegacyPositionalFactory, LegacyRandom, PositionalRandomFactory, RandomSource,
    XoroshiroPositionalFactory, XoroshiroRandom,
};
pub use slot_map::{RawKey, SlotKey, SlotMap};
pub use vec3::Vec3;
