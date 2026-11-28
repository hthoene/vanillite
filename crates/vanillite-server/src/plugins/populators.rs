//! Plugin chunk populators in the world generation: the `chunk-generator` slot's generators are
//! wrapped so that after the terrain step (noise, surface and carvers) the populators of
//! [`gen_stage::AFTER_SURFACE`] and after the features step those of
//! [`gen_stage::AFTER_FEATURES`] change the generated chunks' blocks, in registration order.
//! Without populators nothing is wrapped. They run on the generation threads.

use std::sync::Arc;

use vanillite_abi::gen_stage;
use vanillite_base::ChunkPos;
use vanillite_chunk::slots::{
    ChunkGenerator, ChunkGeneratorProvider, ChunkStatus, Neighbours, ProtoChunk, WorldInfo,
};
use vanillite_data::block::BlockState;
use vanillite_plugin_host::PluginPopulator;

/// `provider`, its generators running `populators` after their stages; `provider` itself when
/// there are none.
pub(crate) fn with_populators(
    provider: Box<dyn ChunkGeneratorProvider>,
    populators: Vec<PluginPopulator>,
) -> Box<dyn ChunkGeneratorProvider> {
    if populators.is_empty() {
        return provider;
    }
    for p in &populators {
        log::info!("Chunk populator of {} (stage {})", p.plugin(), p.stage());
    }
    Box::new(PopulatingProvider {
        inner: provider,
        populators: populators.into(),
    })
}

struct PopulatingProvider {
    inner: Box<dyn ChunkGeneratorProvider>,
    populators: Arc<[PluginPopulator]>,
}

impl ChunkGeneratorProvider for PopulatingProvider {
    fn create(&self, world: &WorldInfo) -> Result<Arc<dyn ChunkGenerator>, String> {
        Ok(Arc::new(Populating {
            inner: self.inner.create(world)?,
            populators: Arc::clone(&self.populators),
            world: world.clone(),
        }))
    }
}

/// A world's generator with the populators after its steps.
struct Populating {
    inner: Arc<dyn ChunkGenerator>,
    populators: Arc<[PluginPopulator]>,
    world: WorldInfo,
}

impl ChunkGenerator for Populating {
    fn new_chunk(&self, pos: ChunkPos) -> ProtoChunk {
        self.inner.new_chunk(pos)
    }

    fn generate(
        &self,
        status: ChunkStatus,
        targets: &mut [ProtoChunk],
        neighbours: &mut Neighbours<'_>,
    ) {
        self.inner.generate(status, targets, neighbours);
        let stage = match status {
            ChunkStatus::Terrain => gen_stage::AFTER_SURFACE,
            ChunkStatus::Features => gen_stage::AFTER_FEATURES,
            _ => return,
        };
        if self.populators.iter().any(|p| p.stage() == stage) {
            self.populate(stage, targets);
        }
    }

    fn prepare(&self) {
        self.inner.prepare();
    }

    fn release_memory(&self) {
        self.inner.release_memory();
    }

    fn trim_idle(&self) {
        self.inner.trim_idle();
    }

    fn spawns_original_mobs(&self) -> bool {
        self.inner.spawns_original_mobs()
    }
}

impl Populating {
    /// Copies the chunks' block state ids out (`((y - min_y) * 16 + z) * 16 + x`), runs the
    /// populators of `stage` on them in order and writes the changed ids back; invalid ids are
    /// ignored.
    fn populate(&self, stage: u32, targets: &mut [ProtoChunk]) {
        let height = usize::try_from(self.world.height).unwrap_or(0);
        let min_y = self.world.min_y;
        let mut chunks: Vec<(ChunkPos, Vec<u32>)> = targets
            .iter()
            .map(|t| {
                let mut blocks = Vec::with_capacity(256 * height);
                for y in (min_y..).take(height) {
                    for z in 0..16 {
                        for x in 0..16 {
                            let state = t.chunk.block(x, y, z).unwrap_or(self.world.air);
                            blocks.push(u32::from(state));
                        }
                    }
                }
                (t.pos, blocks)
            })
            .collect();
        for populator in self.populators.iter().filter(|p| p.stage() == stage) {
            populator.populate(&self.world, &mut chunks);
        }
        // `Chunk::set_block` leaves unchanged blocks alone.
        for (target, (_, after)) in targets.iter_mut().zip(&chunks) {
            for (y, layer) in (min_y..).zip(after.chunks(256)) {
                for (z, row) in (0u32..).zip(layer.chunks(16)) {
                    for (x, &state) in (0u32..).zip(row) {
                        if let Ok(id) = u16::try_from(state)
                            && BlockState(id).is_valid()
                        {
                            target.chunk.set_block(x, y, z, id);
                        }
                    }
                }
            }
        }
    }
}
