//! Light of a chunk whose blocks a neighbour's features change after its light was initialised
//! (#310): a tree that grows across a chunk border into a chunk that already reached
//! `InitializeLight`, next to one that is already lit. Vanilla's `ProtoChunk.setBlockState`
//! updates the light there (`updateSectionStatus`, `checkBlock`); the light must end the same as
//! when the whole area is lit from its final blocks.

mod common;

use std::sync::Arc;

use vanillite_base::ChunkPos;
use vanillite_chunk::slots::{
    ChunkGenerator, ChunkStatus, FlatGenerator, LightEngine, Neighbours, NullCodec, NullStore,
    ProtoChunk,
};
use vanillite_data::block::ids;
use vanillite_light::BfsLightEngine;
use vanillite_world::chunks::{ChunkSettings, ChunkSlots, Chunks, InlineExecutor};

/// The chunk the tree grows in.
const TREE: ChunkPos = ChunkPos::new(0, 0);

/// A flat world whose chunk [`TREE`] gets a canopy of leaves at its features step, high above
/// the ground (in a section that is empty everywhere else) and reaching into the chunk east of
/// it.
struct Canopy(FlatGenerator);

impl ChunkGenerator for Canopy {
    fn new_chunk(&self, pos: ChunkPos) -> ProtoChunk {
        self.0.new_chunk(pos)
    }

    fn generate(&self, status: ChunkStatus, targets: &mut [ProtoChunk], n: &mut Neighbours<'_>) {
        self.0.generate(status, targets, n);
        if status != ChunkStatus::Features {
            return;
        }
        for target in targets.iter_mut().filter(|t| t.pos == TREE) {
            let leaves = ids::OAK_LEAVES.default_state().0;
            for x in 9..23 {
                for z in 3..12 {
                    for y in -40..-36 {
                        let pos = ChunkPos::new(x >> 4, z >> 4);
                        let chunk = if pos == target.pos {
                            &mut *target
                        } else {
                            n.get_mut(pos).expect("written neighbour")
                        };
                        chunk
                            .chunk
                            .set_block((x & 15) as u32, y, (z & 15) as u32, leaves);
                    }
                }
            }
        }
    }

    fn spawns_original_mobs(&self) -> bool {
        false
    }
}

fn run(chunks: &mut Chunks) {
    for _ in 0..8 {
        chunks.tick();
        chunks.end_tick();
    }
}

#[test]
fn features_written_into_initialised_neighbours_relight_them() {
    common::install();
    let dir = std::env::temp_dir();
    let info = common::world_info(&dir);
    let engine = Arc::new(BfsLightEngine::new(&info));
    let slots = ChunkSlots {
        store: Arc::new(NullStore),
        codec: Arc::new(NullCodec),
        generator: Arc::new(Canopy(FlatGenerator::classic(&info))),
        light: Arc::clone(&engine) as Arc<dyn LightEngine>,
        executor: Arc::new(InlineExecutor),
    };
    let settings = ChunkSettings {
        view_distance: 2,
        simulation_distance: 2,
        ..ChunkSettings::default()
    };
    let mut chunks = Chunks::new(info, settings, slots);
    let full = chunks.player_ticket_level();
    // First the chunk two east of the tree becomes full: the chunk east of the tree is at
    // `InitializeLight`, already lit by it, while the tree's chunk is only at `Terrain`.
    chunks.add_ticket(ChunkPos::new(2, 0), 33);
    run(&mut chunks);
    // Then the tree grows into it, and everything around becomes full.
    chunks.add_ticket(TREE, full);
    chunks.add_ticket(ChunkPos::new(1, 0), full);
    run(&mut chunks);

    // The reference: the 3 × 3 around the east chunk lit from the final blocks of the 5 × 5.
    let east = ChunkPos::new(1, 0);
    let mut area: Vec<Arc<ProtoChunk>> = Vec::new();
    for dz in -2..=2 {
        for dx in -2..=2 {
            let pos = ChunkPos::new(east.x + dx, east.z + dz);
            let chunk = chunks
                .chunk(pos)
                .unwrap_or_else(|| panic!("{pos:?} not full"));
            area.push(Arc::clone(chunk));
        }
    }
    let mut reference = area.clone();
    engine.relight_area(east, &mut Neighbours::new(&mut reference));
    let reference = Neighbours::new(&mut reference);
    let mut wrong = Vec::new();
    for dz in -1..=1 {
        for dx in -1..=1 {
            let pos = ChunkPos::new(east.x + dx, east.z + dz);
            let ours = chunks.chunk(pos).unwrap().chunk.light();
            let theirs = reference.get(pos).unwrap().chunk.light();
            for (kind, a, b) in [
                ("sky", ours.sky(), theirs.sky()),
                ("block", ours.block(), theirs.block()),
            ] {
                for (layer, (a, b)) in a.iter().zip(b).enumerate() {
                    if a != b {
                        wrong.push(format!(
                            "{kind} light of {pos:?}, layer {layer}: {a:?} != {b:?}"
                        ));
                    }
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
