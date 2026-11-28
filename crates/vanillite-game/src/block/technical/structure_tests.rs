//! Structure and jigsaw blocks: block entity data, saving an area into `generated/`, loading
//! it back and the corner scan.

use std::sync::Arc;

use vanillite_base::BlockPos;
use vanillite_data::block::{BlockState, ids};
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::serverbound::SetStructureBlock;
use vanillite_protocol::types::BlockPos as WirePos;
use vanillite_worldgen::template::StructureTemplateManager;
use vanillite_worldgen::template::structure::StructureTemplate;
use vanillite_worldgen::template::transform::{Mirror, Rotation};

use super::Editor;
use super::jigsaw::{Jigsaw, Joint};
use super::structure_block::{self, Mode, StructureBlock};
use crate::level::tests::flat_level;
use crate::level::{BlockEntity, Level, update};
use crate::player::sink::PacketSink;

struct NullSink;

impl PacketSink for NullSink {
    fn send_body(&self, _: Vec<u8>) -> bool {
        true
    }
    fn send_shared(&self, _: &Arc<SharedPacket>) -> bool {
        true
    }
    fn close(&self) {}
    fn is_closed(&self) -> bool {
        false
    }
    fn latency_ms(&self) -> u32 {
        0
    }
}

fn editor(sink: &NullSink) -> Editor<'_> {
    Editor {
        player: None,
        uuid: 0,
        allowed: true,
        permission_level: 2,
        sink,
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("vanillite-structure-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn level_with_templates(generated: &std::path::Path) -> Level {
    let mut level = flat_level();
    level.set_structure_templates(Arc::new(StructureTemplateManager::for_world(
        Vec::new(),
        generated.to_path_buf(),
    )));
    level
}

fn packet(
    update_type: u8,
    mode: Mode,
    name: &str,
    offset: [i32; 3],
    size: [i32; 3],
) -> SetStructureBlock {
    SetStructureBlock {
        pos: WirePos {
            x: BLOCK.x,
            y: BLOCK.y,
            z: BLOCK.z,
        },
        update_type,
        mode: match mode {
            Mode::Save => 0,
            Mode::Load => 1,
            Mode::Corner => 2,
            Mode::Data => 3,
        },
        name: name.to_owned(),
        offset,
        size,
        mirror: 0,
        rotation: 0,
        data: String::new(),
        ignore_entities: true,
        strict: false,
        show_air: false,
        show_bounding_box: true,
        integrity: 1.0,
        seed: 0,
    }
}

const BLOCK: BlockPos = BlockPos::new(0, -60, 0);

fn structure_block(mode: Mode) -> BlockState {
    ids::STRUCTURE_BLOCK
        .default_state()
        .with("mode", mode.name())
        .expect("mode")
}

#[test]
fn structure_block_data_round_trips() {
    let block = StructureBlock {
        name: Some("test:house".to_owned()),
        author: "Alex".to_owned(),
        metadata: "chest".to_owned(),
        offset: BlockPos::new(-3, 2, 48),
        size: BlockPos::new(5, 0, 12),
        mirror: Mirror::FrontBack,
        rotation: Rotation::Counterclockwise90,
        mode: Mode::Corner,
        ignore_entities: false,
        strict: true,
        powered: true,
        show_air: true,
        show_bounding_box: false,
        integrity: 0.25,
        seed: -7,
    };
    let mut nbt = Compound::new();
    block.save(&mut nbt);
    assert_eq!(
        nbt.get("rotation"),
        Some(&Tag::String("COUNTERCLOCKWISE_90".into()))
    );
    assert_eq!(nbt.get("mode"), Some(&Tag::String("CORNER".into())));
    let mut loaded = StructureBlock::default();
    loaded.load(&nbt);
    assert_eq!(loaded, block);

    // Missing values: the defaults, `DATA` without a mode; offsets and sizes clamp.
    let mut odd = Compound::new();
    odd.insert("posX", Tag::Int(100));
    odd.insert("sizeY", Tag::Int(-4));
    odd.insert("name", Tag::String("Not Valid".into()));
    let mut loaded = StructureBlock::default();
    loaded.load(&odd);
    assert_eq!(loaded.mode, Mode::Data);
    assert_eq!(loaded.offset, BlockPos::new(48, 1, 0));
    assert_eq!(loaded.size, BlockPos::ZERO);
    assert_eq!(loaded.name, None);
}

#[test]
fn jigsaw_data_round_trips_and_defaults_the_joint() {
    let up = ids::JIGSAW
        .default_state()
        .with("orientation", "up_north")
        .expect("state");
    let mut jigsaw = Jigsaw::new(up);
    jigsaw.name = "minecraft:a".to_owned();
    jigsaw.target = "minecraft:b".to_owned();
    jigsaw.pool = "minecraft:village/plains/streets".to_owned();
    jigsaw.final_state = "minecraft:stone".to_owned();
    jigsaw.joint = Joint::Aligned;
    jigsaw.placement_priority = 3;
    jigsaw.selection_priority = -2;
    let mut nbt = Compound::new();
    jigsaw.save(&mut nbt);
    let mut loaded = Jigsaw::new(up);
    loaded.load(&nbt);
    assert_eq!(loaded, jigsaw);

    // Without a joint the orientation decides: rollable facing up, aligned sideways.
    nbt.remove("joint");
    let mut loaded = Jigsaw::new(up);
    loaded.load(&nbt);
    assert_eq!(loaded.joint, Joint::Rollable);
    let mut loaded = Jigsaw::new(ids::JIGSAW.default_state());
    loaded.load(&nbt);
    assert_eq!(loaded.joint, Joint::Aligned);
}

#[test]
fn saves_an_area_to_generated_and_loads_it_back() {
    let dir = temp_dir("save");
    let generated = dir.join("generated");
    let mut level = level_with_templates(&generated);
    let sink = NullSink;
    let stone = BlockPos::new(1, -60, 0);
    let stairs = BlockPos::new(1, -59, 1);
    let chest = BlockPos::new(2, -60, 1);
    level.set_block(stone, ids::STONE.default_state(), update::ALL);
    level.set_block(stairs, ids::OAK_STAIRS.default_state(), update::ALL);
    level.set_block(chest, ids::CHEST.default_state(), update::ALL);
    level.set_block(BLOCK, structure_block(Mode::Save), update::ALL);
    assert!(matches!(
        level.block_entity(BLOCK),
        Some(BlockEntity::StructureBlock(b)) if b.mode == Mode::Save
    ));

    structure_block::handle(
        &mut level,
        packet(1, Mode::Save, "test:box", [1, 0, 0], [2, 2, 2]),
        &editor(&sink),
    );
    let file = generated.join("test/structure/box.nbt");
    let bytes = std::fs::read(&file).expect("saved structure file");
    let template = StructureTemplate::from_gzip(&bytes).expect("valid file");
    assert_eq!(template.size(), BlockPos::new(2, 2, 2));
    let blocks: Vec<_> = template.palettes()[0].blocks().collect();
    // Every block of the area (air too), full blocks first, block entities last.
    assert_eq!(blocks.len(), 8);
    assert_eq!(blocks[0].state, ids::STONE.default_state());
    assert_eq!(blocks[0].pos, BlockPos::ZERO);
    let last = blocks.last().expect("blocks");
    assert_eq!(last.state.block(), ids::CHEST);
    assert_eq!(last.pos, BlockPos::new(1, 0, 1));
    let nbt = last.nbt.as_ref().expect("chest data");
    assert_eq!(nbt.get("id"), Some(&Tag::String("minecraft:chest".into())));
    assert!(nbt.get("x").is_none());

    // Clear the area, forget the cached template and load it from the file again.
    for pos in [stone, stairs, chest] {
        level.set_block(pos, BlockState::AIR, update::ALL);
    }
    level.forget_structure_template("test:box");
    structure_block::handle(
        &mut level,
        packet(2, Mode::Load, "test:box", [1, 0, 0], [2, 2, 2]),
        &editor(&sink),
    );
    assert_eq!(level.block(BLOCK), structure_block(Mode::Load));
    assert_eq!(level.block(stone), ids::STONE.default_state());
    assert_eq!(level.block(stairs).block(), ids::OAK_STAIRS);
    assert_eq!(level.block(chest).block(), ids::CHEST);
    assert!(level.block_entity(chest).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_load_with_another_size_only_takes_the_size() {
    let dir = temp_dir("prepare");
    let mut level = level_with_templates(&dir.join("generated"));
    let sink = NullSink;
    let mut template = StructureTemplate::filled(BlockPos::new(3, 1, 1), Vec::new(), Vec::new());
    template.set_author("Steve");
    level.set_structure_template("test:sized", template);
    level.set_block(BLOCK, structure_block(Mode::Load), update::ALL);
    structure_block::handle(
        &mut level,
        packet(2, Mode::Load, "test:sized", [0, 1, 0], [1, 1, 1]),
        &editor(&sink),
    );
    let Some(BlockEntity::StructureBlock(block)) = level.block_entity(BLOCK) else {
        panic!("structure block");
    };
    assert_eq!(block.size, BlockPos::new(3, 1, 1));
    assert_eq!(block.author, "Steve");
}

#[test]
fn the_corner_scan_finds_the_box_between_the_corners() {
    let mut level = flat_level();
    let sink = NullSink;
    for corner in [BlockPos::new(3, -60, 3), BlockPos::new(6, -56, 7)] {
        level.set_block(corner, structure_block(Mode::Corner), update::ALL);
        if let Some(BlockEntity::StructureBlock(b)) = level.block_entity_mut(corner) {
            b.set_name("test:box");
        }
    }
    level.set_block(BLOCK, structure_block(Mode::Save), update::ALL);
    structure_block::handle(
        &mut level,
        packet(3, Mode::Save, "test:box", [0, 1, 0], [0, 0, 0]),
        &editor(&sink),
    );
    let Some(BlockEntity::StructureBlock(block)) = level.block_entity(BLOCK) else {
        panic!("structure block");
    };
    assert_eq!(block.offset, BlockPos::new(4, 1, 4));
    assert_eq!(block.size, BlockPos::new(2, 3, 3));
}

#[test]
fn a_redstone_pulse_loads_in_load_mode() {
    let dir = temp_dir("pulse");
    let mut level = level_with_templates(&dir.join("generated"));
    let stone = BlockPos::new(0, -59, 0);
    let blocks = vec![vanillite_worldgen::template::StructureBlockInfo {
        pos: BlockPos::ZERO,
        state: ids::STONE.default_state(),
        nbt: None,
    }];
    let template = StructureTemplate::filled(BlockPos::new(1, 1, 1), blocks, Vec::new());
    level.set_structure_template("test:one", template);
    level.set_block(BLOCK, structure_block(Mode::Load), update::ALL);
    if let Some(BlockEntity::StructureBlock(b)) = level.block_entity_mut(BLOCK) {
        b.set_name("test:one");
    }
    level.set_block(
        BLOCK.offset(1, 0, 0),
        ids::REDSTONE_BLOCK.default_state(),
        update::ALL,
    );
    assert_eq!(level.block(stone), ids::STONE.default_state());
}
