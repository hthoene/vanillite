//! Golden translations of the steps below 26.2: for each step, packets whose format changed in
//! it, from what the server (26.3) sends down to a client of the step's older version. The
//! 26.3 side is written from `data/vanilla/`, the expected bytes by hand from the older wire
//! format with ids from `data/multiversion/<version>/`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]

use serde_json::Value;
use vanillite_multiversion::{Accepted, ClientboundHalf, ServerboundHalf, State};
use vanillite_protocol::codec::{write_str, write_varint};

fn json(path: &str) -> Value {
    let p = vanillite_multiversion::data_paths::file(path);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Ids of an older version and of the server's version by name.
struct Ids {
    protocol: i32,
    old: Value,
    new: Value,
    old_packets: Value,
    new_packets: Value,
}

impl Ids {
    fn load(dir: &str) -> Self {
        let version = std::fs::read_to_string(vanillite_multiversion::data_paths::file(&format!(
            "data/multiversion/{dir}/version.toml"
        )))
        .unwrap();
        Self {
            protocol: version
                .lines()
                .find_map(|l| l.strip_prefix("protocol = "))
                .unwrap()
                .parse()
                .unwrap(),
            old: json(&format!("data/multiversion/{dir}/registries.json")),
            new: json("data/vanilla/registries.json"),
            old_packets: json(&format!("data/multiversion/{dir}/packets.json")),
            new_packets: json("data/vanilla/packets.json"),
        }
    }

    fn find(list: &Value, name: &str) -> i32 {
        let name = if name.contains(':') {
            name.to_owned()
        } else {
            format!("minecraft:{name}")
        };
        let pos = list
            .as_array()
            .unwrap()
            .iter()
            .position(|e| e.as_str() == Some(name.as_str()))
            .unwrap_or_else(|| panic!("{name} not found"));
        i32::try_from(pos).unwrap()
    }

    fn old_id(&self, registry: &str, name: &str) -> i32 {
        Self::find(&self.old[registry]["entries"], name)
    }

    fn new_id(&self, registry: &str, name: &str) -> i32 {
        Self::find(&self.new[registry]["entries"], name)
    }

    /// A clientbound play packet of the server's version.
    fn new_cb(&self, name: &str, fields: &[u8]) -> Vec<u8> {
        body(
            Self::find(&self.new_packets["play"]["clientbound"], name),
            fields,
        )
    }

    /// A clientbound play packet of the older version.
    fn old_cb(&self, name: &str, fields: &[u8]) -> Vec<u8> {
        body(
            Self::find(&self.old_packets["play"]["clientbound"], name),
            fields,
        )
    }

    fn new_sb(&self, name: &str, fields: &[u8]) -> Vec<u8> {
        body(
            Self::find(&self.new_packets["play"]["serverbound"], name),
            fields,
        )
    }

    fn old_sb(&self, name: &str, fields: &[u8]) -> Vec<u8> {
        body(
            Self::find(&self.old_packets["play"]["serverbound"], name),
            fields,
        )
    }

    fn halves(&self) -> (ServerboundHalf, ClientboundHalf) {
        Accepted::all()
            .adapter(self.protocol, State::Play)
            .expect("supported")
            .split()
    }
}

fn body(id: i32, fields: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    write_varint(&mut b, id);
    b.extend_from_slice(fields);
    b
}

fn cb(half: &mut ClientboundHalf, body: &[u8]) -> Option<Vec<u8>> {
    half.translate(body)
        .expect("translates")
        .map(<[u8]>::to_vec)
}

fn sb(half: &mut ServerboundHalf, body: &[u8]) -> Option<Vec<u8>> {
    half.translate(body)
        .expect("translates")
        .map(<[u8]>::to_vec)
}

fn varint(v: i32) -> Vec<u8> {
    let mut b = Vec::new();
    write_varint(&mut b, v);
    b
}

fn f64s(v: [f64; 3]) -> Vec<u8> {
    v.iter().flat_map(|c| c.to_be_bytes()).collect()
}

/// The server's `login`: entity 5, one dimension, spawn info with sea level, online mode.
fn new_login() -> Vec<u8> {
    let mut b = 5i32.to_be_bytes().to_vec();
    b.push(0);
    b.push(1);
    write_str(&mut b, "minecraft:overworld");
    b.extend([20, 2, 2, 0, 1, 0]);
    // Spawn info (26.3): dimension type, name, seed, game mode, previous (none), debug, flat,
    // no death location, portal cooldown, sea level.
    b.push(0);
    write_str(&mut b, "minecraft:overworld");
    b.extend([0; 8]);
    b.extend([1, 0, 0, 0, 0, 0]);
    b.extend(varint(63));
    b.push(1); // online mode
    b.push(0); // secure chat
    b
}

/// The same `login` for a client of `protocol`.
fn old_login(protocol: i32) -> Vec<u8> {
    let mut b = 5i32.to_be_bytes().to_vec();
    b.push(0);
    b.push(1);
    write_str(&mut b, "minecraft:overworld");
    b.extend([20, 2, 2, 0, 1, 0]);
    b.push(0);
    write_str(&mut b, "minecraft:overworld");
    b.extend([0; 8]);
    // Game mode and previous game mode as bytes (-1: none) before 26.3.
    b.extend([1, 0xff, 0, 0, 0, 0]);
    if protocol >= 768 {
        b.extend(varint(63));
    }
    if protocol >= 776 {
        b.push(1);
    }
    b.push(0);
    b
}

#[test]
fn login_down_every_step() {
    for dir in [
        "26.1.2", "1.21.11", "1.21.10", "1.21.8", "1.21.6", "1.21.5", "1.21.4", "1.21.3", "1.21.1",
        "1.20.6",
    ] {
        let ids = Ids::load(dir);
        let (_, mut c) = ids.halves();
        assert_eq!(
            cb(&mut c, &ids.new_cb("login", &new_login())),
            Some(ids.old_cb("login", &old_login(ids.protocol))),
            "login for {dir}"
        );
    }
}

/// Step 26.1 ↔ 26.2: team parameters in the old order with a colour enum (`RESET` for none).
#[test]
fn step_26_1_team() {
    let ids = Ids::load("26.1.2");
    let (_, mut c) = ids.halves();
    let text = |s: &str| {
        let mut b = vec![8];
        b.extend((s.len() as u16).to_be_bytes());
        b.extend(s.as_bytes());
        b
    };
    let mut new = Vec::new();
    write_str(&mut new, "red");
    new.push(0);
    new.extend(text("Red"));
    new.extend(text("["));
    new.extend(text("]"));
    new.extend([1, 2]); // never, push other teams
    new.extend([1, 12]); // colour red
    new.push(3); // options
    new.push(0); // no players
    let mut old = Vec::new();
    write_str(&mut old, "red");
    old.push(0);
    old.extend(text("Red"));
    old.push(3);
    old.extend([1, 2, 12]);
    old.extend(text("["));
    old.extend(text("]"));
    old.push(0);
    assert_eq!(
        cb(&mut c, &ids.new_cb("set_player_team", &new)),
        Some(ids.old_cb("set_player_team", &old))
    );
}

/// Step 1.21.11 ↔ 26.1: interactions split into `attack` and `interact` with a position.
#[test]
fn step_1_21_11_interact() {
    let ids = Ids::load("1.21.11");
    let (mut s, _) = ids.halves();
    // Attack (action 1): entity 9.
    assert_eq!(
        sb(&mut s, &ids.old_sb("interact", &[9, 1, 0])),
        Some(ids.new_sb("attack", &[9]))
    );
    // Interact at (2) a position, then the plain interact the old client sends after it.
    let mut at = vec![9, 2];
    for v in [0.5f32, 1.0, 0.0] {
        at.extend(v.to_be_bytes());
    }
    at.extend([0, 1]);
    let sent = sb(&mut s, &ids.old_sb("interact", &at)).unwrap();
    assert_eq!(&sent[..3], &ids.new_sb("interact", &[9, 0])[..]);
    assert_eq!(*sent.last().unwrap(), 1, "sneaking");
    assert_eq!(sb(&mut s, &ids.old_sb("interact", &[9, 0, 0, 1])), None);
    // A plain interact on its own.
    assert_eq!(
        sb(&mut s, &ids.old_sb("interact", &[9, 0, 1, 0])),
        Some(ids.new_sb("interact", &[9, 1, 0, 0]))
    );
}

/// `client_command` (respawn, statistics) reaches the server from every version: 26.1 only
/// appended an action, so older clients could not respawn when the step dropped the packet.
#[test]
fn client_command_every_version() {
    for v in vanillite_multiversion::SUPPORTED {
        let ids = Ids::load(v.id);
        let (mut s, _) = ids.halves();
        for action in [0, 1] {
            assert_eq!(
                sb(&mut s, &ids.old_sb("client_command", &[action])),
                Some(ids.new_sb("client_command", &[action])),
                "{} action {action}",
                v.id
            );
        }
    }
}

/// Entity types an older version lacks show as a stand-in instead of not at all.
#[test]
fn new_entity_types_have_stand_ins_every_version() {
    let cases: &[(&str, &[&str])] = &[
        ("parched", &["skeleton"]),
        ("camel_husk", &["camel"]),
        ("zombie_nautilus", &["nautilus", "turtle"]),
        ("nautilus", &["turtle"]),
        ("sulfur_cube", &["slime"]),
        ("happy_ghast", &["ghast"]),
        ("copper_golem", &["iron_golem"]),
        ("mannequin", &["armor_stand"]),
        ("creaking", &["enderman"]),
        ("bamboo_raft", &["boat"]),
        ("cushion", &["interaction"]),
    ];
    for v in vanillite_multiversion::SUPPORTED {
        let ids = Ids::load(v.id);
        let old = ids.old["minecraft:entity_type"]["entries"]
            .as_array()
            .unwrap();
        let has = |n: &str| {
            old.iter()
                .any(|e| e.as_str() == Some(&format!("minecraft:{n}")))
        };
        let (_, mut c) = ids.halves();
        for (name, stand_ins) in cases {
            let want = std::iter::once(name)
                .chain(stand_ins.iter())
                .find(|n| has(n))
                .unwrap_or_else(|| panic!("{}: no stand-in for {name}", v.id));
            let mut new = varint(3);
            new.extend([0x11; 16]);
            new.extend(varint(ids.new_id("minecraft:entity_type", name)));
            new.extend(f64s([1.0, 2.0, 3.0]));
            new.push(0);
            new.extend([4, 5, 6]);
            new.push(0);
            let out = cb(&mut c, &ids.new_cb("add_entity", &new))
                .unwrap_or_else(|| panic!("{}: {name} not sent", v.id));
            let mut r = vanillite_protocol::codec::Reader::new(&out);
            r.varint().unwrap();
            r.varint().unwrap();
            r.skip(16).unwrap();
            assert_eq!(
                r.varint().unwrap(),
                ids.old_id("minecraft:entity_type", want),
                "{}: {name} should show as {want}",
                v.id
            );
        }
    }
}

/// Items an older version lacks show as a related item rather than as stone.
#[test]
fn new_items_have_stand_ins() {
    use vanillite_multiversion::generated as g;
    let cases = [
        (
            &g::v1_21_10::TABLES,
            "1.21.10",
            "1.21.11",
            "netherite_spear",
            "netherite_sword",
        ),
        (
            &g::v1_21_10::TABLES,
            "1.21.10",
            "1.21.11",
            "copper_spear",
            "copper_sword",
        ),
        (
            &g::v1_21_5::TABLES,
            "1.21.5",
            "1.21.6",
            "white_harness",
            "saddle",
        ),
        (
            &g::v1_21_5::TABLES,
            "1.21.5",
            "1.21.6",
            "music_disc_tears",
            "music_disc_13",
        ),
        (
            &g::v1_21_8::TABLES,
            "1.21.8",
            "1.21.10",
            "copper_golem_statue",
            "copper_block",
        ),
        (
            &g::v1_21_5::TABLES,
            "1.21.5",
            "1.21.6",
            "dried_ghast",
            "soul_sand",
        ),
        (
            &g::v1_21_4::TABLES,
            "1.21.4",
            "1.21.5",
            "wildflowers",
            "pink_petals",
        ),
    ];
    for (step, older, newer, item, want) in cases {
        let (older, newer) = (Ids::load(older), Ids::load(newer));
        assert_eq!(
            step.older("minecraft:item", newer.old_id("minecraft:item", item)),
            Some(older.old_id("minecraft:item", want)),
            "{item}"
        );
    }
    // 26.3 content for 26.2 clients.
    let older = Ids::load("26.2");
    for (item, want) in [
        ("red_shrub", "dead_bush"),
        ("white_cushion", "white_carpet"),
    ] {
        assert_eq!(
            g::v26_2::TABLES.older("minecraft:item", older.new_id("minecraft:item", item)),
            Some(older.old_id("minecraft:item", want)),
            "{item}"
        );
    }
}

/// Step 1.21.8 ↔ 1.21.9: velocities as three shorts at the end of `add_entity`.
#[test]
fn step_1_21_8_velocity() {
    let ids = Ids::load("1.21.8");
    let (_, mut c) = ids.halves();
    let mut new = varint(3);
    new.extend([0x11; 16]);
    new.extend(varint(ids.new_id("minecraft:entity_type", "minecraft:pig")));
    new.extend(f64s([1.0, 2.0, 3.0]));
    new.push(0); // no movement
    new.extend([4, 5, 6]);
    new.push(0);
    let mut old = varint(3);
    old.extend([0x11; 16]);
    old.extend(varint(ids.old_id("minecraft:entity_type", "minecraft:pig")));
    old.extend(f64s([1.0, 2.0, 3.0]));
    old.extend([4, 5, 6]);
    old.push(0);
    old.extend([0; 6]);
    assert_eq!(
        cb(&mut c, &ids.new_cb("add_entity", &new)),
        Some(ids.old_cb("add_entity", &old))
    );
    // Respawn data: dimension, position, yaw, pitch → position, angle.
    let mut spawn = Vec::new();
    write_str(&mut spawn, "minecraft:overworld");
    spawn.extend(7i64.to_be_bytes());
    spawn.extend(90.0f32.to_be_bytes());
    spawn.extend(0.0f32.to_be_bytes());
    let mut want = 7i64.to_be_bytes().to_vec();
    want.extend(90.0f32.to_be_bytes());
    assert_eq!(
        cb(&mut c, &ids.new_cb("set_default_spawn_position", &spawn)),
        Some(ids.old_cb("set_default_spawn_position", &want))
    );
}

/// Steps 1.21.5 ↔ 1.21.6 and down: shift comes as player commands, difficulty as a byte.
#[test]
fn step_1_21_5_commands() {
    let ids = Ids::load("1.21.5");
    let (mut s, mut c) = ids.halves();
    // Press shift (0) is gone from 1.21.6 on; stop sleeping (2) becomes 0.
    assert_eq!(sb(&mut s, &ids.old_sb("player_command", &[1, 0, 0])), None);
    assert_eq!(
        sb(&mut s, &ids.old_sb("player_command", &[1, 2, 0])),
        Some(ids.new_sb("player_command", &[1, 0, 0]))
    );
    assert_eq!(
        cb(&mut c, &ids.new_cb("change_difficulty", &[2, 1])),
        Some(ids.old_cb("change_difficulty", &[2, 1]))
    );
}

/// Step 1.21.4 ↔ 1.21.5: heightmaps as NBT, palette data with its length, player chat without
/// the global index, container clicks with full stacks.
#[test]
fn step_1_21_4_chunk_and_click() {
    let ids = Ids::load("1.21.4");
    let (mut s, mut c) = ids.halves();
    let chunk = |old: bool| {
        let mut b = Vec::new();
        b.extend(1i32.to_be_bytes());
        b.extend(2i32.to_be_bytes());
        if old {
            // Compound { MOTION_BLOCKING: long[1] }.
            b.push(10);
            b.push(12);
            b.extend((15u16).to_be_bytes());
            b.extend(b"MOTION_BLOCKING");
            b.extend(1i32.to_be_bytes());
            b.extend(9i64.to_be_bytes());
            b.push(0);
        } else {
            b.extend([1, 4, 1]);
            b.extend(9i64.to_be_bytes());
        }
        // One section of air with plains-like biome 0; 26.3 also counts fluids.
        let mut sec = vec![0, 0];
        if !old {
            sec.extend([0, 0]);
        }
        sec.extend([0, 0]);
        if old {
            sec.push(0);
        }
        sec.extend([0, 0]);
        if old {
            sec.push(0);
        }
        b.extend(varint(sec.len() as i32));
        b.extend(sec);
        b.push(0);
        // Light: four empty masks (long arrays before 26.3), no arrays.
        b.extend([0, 0, 0, 0, 0, 0]);
        b
    };
    assert_eq!(
        cb(&mut c, &ids.new_cb("level_chunk_with_light", &chunk(false))),
        Some(ids.old_cb("level_chunk_with_light", &chunk(true)))
    );

    // Container click: one changed slot with a full stack → a hashed stack without
    // component hashes; the carried item empty.
    let stone_old = ids.old_id("minecraft:item", "minecraft:stone");
    let stone_new = ids.new_id("minecraft:item", "minecraft:stone");
    let mut click = vec![1, 0, 0, 5, 0, 0, 1];
    click.extend(5i16.to_be_bytes());
    click.extend(varint(3));
    click.extend(varint(stone_old));
    click.extend([0, 0, 0]);
    let mut want = vec![1, 0, 0, 5, 0, 0, 1];
    want.extend(5i16.to_be_bytes());
    want.push(1);
    want.extend(varint(stone_new));
    want.extend(varint(3));
    want.extend([0, 0, 0]);
    let got = sb(&mut s, &ids.old_sb("container_click", &click));
    assert_eq!(got, Some(ids.new_sb("container_click", &want)));
}

/// Step 1.21.3 ↔ 1.21.4: the held slot as a byte; `player_loaded` follows the teleport
/// confirmation.
#[test]
fn step_1_21_3_held_slot_and_loaded() {
    let ids = Ids::load("1.21.3");
    let (mut s, mut c) = ids.halves();
    assert_eq!(
        cb(&mut c, &ids.new_cb("set_held_slot", &[4])),
        Some(ids.old_cb("set_held_slot", &[4]))
    );
    assert_eq!(
        sb(&mut s, &ids.old_sb("accept_teleportation", &[7])),
        Some(ids.new_sb("accept_teleportation", &{
            let mut b = vec![7];
            b.extend([0; 24 + 8]);
            b
        }))
    );
    assert_eq!(
        s.pending().map(<[u8]>::to_vec),
        Some(ids.new_sb("player_loaded", &[]))
    );
}

/// Step 1.21.1 ↔ 1.21.2: player positions in the old layout, container ids as bytes, the
/// cursor item as a slot of container -1, list order left out of the tab list, the end of
/// each tick after movements.
#[test]
fn step_1_21_1_positions_and_containers() {
    let ids = Ids::load("1.21.1");
    let (mut s, mut c) = ids.halves();
    let mut pos = varint(3);
    pos.extend(f64s([1.0, 64.0, -2.0]));
    pos.extend([0; 24]);
    pos.extend(90.0f32.to_be_bytes());
    pos.extend(5.0f32.to_be_bytes());
    pos.extend(0x18i32.to_be_bytes());
    let mut want = f64s([1.0, 64.0, -2.0]);
    want.extend(90.0f32.to_be_bytes());
    want.extend(5.0f32.to_be_bytes());
    want.push(0x18);
    want.push(3);
    assert_eq!(
        cb(&mut c, &ids.new_cb("player_position", &pos)),
        Some(ids.old_cb("player_position", &want))
    );

    // Cursor: container -1, state 0, slot -1, empty.
    let mut want = vec![0xff, 0];
    want.extend((-1i16).to_be_bytes());
    want.push(0);
    assert_eq!(
        cb(&mut c, &ids.new_cb("set_cursor_item", &[0])),
        Some(ids.old_cb("container_set_slot", &want))
    );

    // Tab list: add player + list order (bit 6) → add player only.
    let mut info = vec![0x41, 1];
    info.extend([0xaa; 16]);
    write_str(&mut info, "Bob");
    info.push(0);
    info.push(9);
    let mut want = vec![0x01, 1];
    want.extend([0xaa; 16]);
    write_str(&mut want, "Bob");
    want.push(0);
    assert_eq!(
        cb(&mut c, &ids.new_cb("player_info_update", &info)),
        Some(ids.old_cb("player_info_update", &want))
    );

    // Movement, then the end of the client tick the server expects.
    let mut walk = f64s([1.0, 64.0, -2.0]);
    walk.push(1);
    assert_eq!(
        sb(&mut s, &ids.old_sb("move_player_pos", &walk)),
        Some(ids.new_sb("move_player_pos", &walk))
    );
    assert_eq!(
        s.pending().map(<[u8]>::to_vec),
        Some(ids.new_sb("client_tick_end", &[]))
    );
    assert_eq!(s.pending(), None);

    // Pressing shift: the command is gone from 1.21.6 on, the player input carries it.
    assert_eq!(
        sb(&mut s, &ids.old_sb("player_command", &[1, 0, 0])),
        Some(ids.new_sb("player_input", &[32]))
    );
}

/// Step 1.20.5 ↔ 1.21: attribute modifiers by UUID (sprinting keeps the client's own), items
/// used with the player's rotation.
#[test]
fn step_1_20_5_attributes_and_use_item() {
    let ids = Ids::load("1.20.6");
    let (mut s, mut c) = ids.halves();
    let speed_new = ids.new_id("minecraft:attribute", "minecraft:movement_speed");
    let speed_old = ids.old_id("minecraft:attribute", "minecraft:generic.movement_speed");
    let mut attrs = varint(5);
    attrs.push(1);
    attrs.extend(varint(speed_new));
    attrs.extend(0.1f64.to_be_bytes());
    attrs.push(1);
    write_str(&mut attrs, "minecraft:sprinting");
    attrs.extend(0.3f64.to_be_bytes());
    attrs.push(2);
    let mut want = varint(5);
    want.push(1);
    want.extend(varint(speed_old));
    want.extend(0.1f64.to_be_bytes());
    want.push(1);
    want.extend(0x662a_6b8d_da3e_4c1c_8813_96ea_6097_278d_u128.to_be_bytes());
    want.extend(0.3f64.to_be_bytes());
    want.push(2);
    assert_eq!(
        cb(&mut c, &ids.new_cb("update_attributes", &attrs)),
        Some(ids.old_cb("update_attributes", &want))
    );

    // The rotation from the last movement goes with `use_item`.
    let mut rot = 45.0f32.to_be_bytes().to_vec();
    rot.extend(10.0f32.to_be_bytes());
    rot.push(1);
    sb(&mut s, &ids.old_sb("move_player_rot", &rot)).unwrap();
    let mut want = vec![0, 3];
    want.extend(45.0f32.to_be_bytes());
    want.extend(10.0f32.to_be_bytes());
    assert_eq!(
        sb(&mut s, &ids.old_sb("use_item", &[0, 3])),
        Some(ids.new_sb("use_item", &want))
    );
}

/// Entity data of players follows the layouts: main hand and skin parts moved with 1.21.9,
/// the main hand became an enum with 1.21.11.
#[test]
fn player_entity_data() {
    for (dir, hand, skin) in [("26.1.2", 15, 16), ("1.21.10", 15, 16), ("1.20.6", 18, 17)] {
        let ids = Ids::load(dir);
        let (_, mut c) = ids.halves();
        // The player's own entity, known from `login`.
        cb(&mut c, &ids.new_cb("login", &new_login())).unwrap();
        // 26.3: main hand (15, humanoid arm 42) right, skin parts (16, byte 0) 0x7f.
        let data = ids.new_cb("set_entity_data", &[5, 15, 42, 1, 16, 0, 0x7f, 0xff]);
        let got = cb(&mut c, &data).unwrap();
        let hand_bytes = if dir == "26.1.2" {
            // Still a humanoid arm: its serializer id in 26.1.
            let serializers = json(&format!("data/multiversion/{dir}/entity_data.json"));
            let id = serializers["serializers"]
                .as_array()
                .unwrap()
                .iter()
                .position(|s| s == "humanoid_arm")
                .unwrap();
            vec![hand, id as u8, 1]
        } else {
            vec![hand, 0, 1]
        };
        let mut want = hand_bytes;
        want.extend([skin, 0, 0x7f, 0xff]);
        let want = ids.old_cb("set_entity_data", &[&[5][..], &want[..]].concat());
        assert_eq!(got, want, "player entity data for {dir}");
    }
}

/// Every older version, newest first.
const ALL: [&str; 11] = [
    "26.2", "26.1.2", "1.21.11", "1.21.10", "1.21.8", "1.21.6", "1.21.5", "1.21.4", "1.21.3",
    "1.21.1", "1.20.6",
];

/// `minecraft:slot_display` id of `name` in a version (26.1 added three types).
fn slot(protocol: i32, name: &str) -> Vec<u8> {
    let new = [
        "empty",
        "any_fuel",
        "with_any_potion",
        "only_with_component",
        "item",
        "item_stack",
        "tag",
        "dyed",
        "smithing_trim",
        "with_remainder",
        "composite",
    ];
    let old = [
        "empty",
        "any_fuel",
        "item",
        "item_stack",
        "tag",
        "smithing_trim",
        "with_remainder",
        "composite",
    ];
    let list: &[&str] = if protocol >= 775 { &new } else { &old };
    varint(list.iter().position(|n| *n == name).unwrap() as i32)
}

/// An item stack of `count` without components: a template from 26.1 on, a stack before.
fn stack(protocol: i32, item: i32, count: i32) -> Vec<u8> {
    if protocol >= 775 {
        [varint(item), varint(count), vec![0, 0]].concat()
    } else {
        [varint(count), varint(item), vec![0, 0]].concat()
    }
}

/// The server's recipe book: one shaped recipe (display 7) of planks (a tag as a direct item
/// list) over a stick, made at a crafting table, giving four sticks.
fn new_book_display(ids: &Ids) -> Vec<u8> {
    let item = |n: &str| varint(ids.new_id("minecraft:item", n));
    [
        varint(1),
        varint(1),
        varint(2),
        varint(2),
        varint(6),
        varint(2),
        item("oak_planks"),
        varint(4),
        item("stick"),
        varint(5),
        item("stick"),
        varint(4),
        vec![0, 0],
        varint(4),
        item("crafting_table"),
    ]
    .concat()
}

fn new_book(ids: &Ids) -> Vec<u8> {
    let category = varint(ids.new_id("minecraft:recipe_book_category", "crafting_misc"));
    [
        varint(1),
        varint(7),
        new_book_display(ids),
        varint(0),
        category,
        vec![0, 3, 1],
    ]
    .concat()
}

/// Recipe book entries reach every older version: from 1.21.2 on as displays with the older
/// item ids and slot display types (tags as item lists), before as full recipes named after
/// the display and unlocked with `recipe`; ghost recipes and recipe placement name them.
#[test]
fn recipe_book_down_every_step() {
    for dir in ALL {
        let ids = Ids::load(dir);
        let p = ids.protocol;
        let (mut s, mut c) = ids.halves();
        let item = |n: &str| ids.old_id("minecraft:item", n);
        let got = cb(&mut c, &ids.new_cb("recipe_book_add", &new_book(&ids)));
        if p >= 768 {
            let display = [
                varint(1),
                varint(1),
                varint(2),
                varint(2),
                slot(p, "composite"),
                varint(1),
                slot(p, "item"),
                varint(item("oak_planks")),
                slot(p, "item"),
                varint(item("stick")),
                slot(p, "item_stack"),
                stack(p, item("stick"), 4),
                slot(p, "item"),
                varint(item("crafting_table")),
            ]
            .concat();
            let category = ids.old_id("minecraft:recipe_book_category", "crafting_misc");
            let want = [
                varint(1),
                varint(7),
                display.clone(),
                varint(0),
                varint(category),
                vec![0, 3, 1],
            ]
            .concat();
            assert_eq!(got, Some(ids.old_cb("recipe_book_add", &want)), "{dir}");
            assert_eq!(c.pending(), None, "{dir}");
            let ghost = cb(
                &mut c,
                &ids.new_cb(
                    "place_ghost_recipe",
                    &[varint(1), new_book_display(&ids)].concat(),
                ),
            );
            assert_eq!(
                ghost,
                Some(ids.old_cb("place_ghost_recipe", &[varint(1), display].concat())),
                "{dir}"
            );
            continue;
        }
        let one = |i: i32| [varint(1), varint(1), varint(i), vec![0, 0]].concat();
        let mut recipes = varint(1);
        write_str(&mut recipes, "vanillite:book/7");
        recipes.extend(varint(0));
        write_str(&mut recipes, "");
        recipes.extend([varint(3), varint(1), varint(2)].concat());
        recipes.extend(one(item("oak_planks")));
        recipes.extend(one(item("stick")));
        recipes.extend([varint(4), varint(item("stick")), vec![0, 0, 1]].concat());
        assert_eq!(got, Some(ids.old_cb("update_recipes", &recipes)), "{dir}");
        let mut unlock = varint(0);
        unlock.extend([0; 8]);
        for _ in 0..2 {
            unlock.extend(varint(1));
            write_str(&mut unlock, "vanillite:book/7");
        }
        assert_eq!(
            c.pending().map(<[u8]>::to_vec),
            Some(ids.old_cb("recipe", &unlock)),
            "{dir}"
        );
        assert_eq!(c.pending(), None, "{dir}");
        let ghost = cb(
            &mut c,
            &ids.new_cb(
                "place_ghost_recipe",
                &[varint(1), new_book_display(&ids)].concat(),
            ),
        );
        let mut want = vec![1];
        write_str(&mut want, "vanillite:book/7");
        assert_eq!(
            ghost,
            Some(ids.old_cb("place_ghost_recipe", &want)),
            "{dir}"
        );
        let mut place = vec![1];
        write_str(&mut place, "vanillite:book/7");
        place.push(1);
        assert_eq!(
            sb(&mut s, &ids.old_sb("place_recipe", &place)),
            Some(ids.new_sb("place_recipe", &[1, 7, 1])),
            "{dir}"
        );
    }
}

/// Stonecutter recipes and recipe item sets reach every older version.
#[test]
fn stonecutter_recipes_down_every_step() {
    for dir in ALL {
        let ids = Ids::load(dir);
        let p = ids.protocol;
        let (_, mut c) = ids.halves();
        let new = |n: &str| varint(ids.new_id("minecraft:item", n));
        let old = |n: &str| ids.old_id("minecraft:item", n);
        let mut fields = varint(1);
        write_str(&mut fields, "minecraft:furnace_input");
        fields.extend([varint(1), new("stone")].concat());
        fields.extend([varint(1), varint(2), new("stone")].concat());
        fields.extend([varint(5), new("stone_slab"), varint(2), vec![0, 0]].concat());
        let got = cb(&mut c, &ids.new_cb("update_recipes", &fields));
        let want = if p >= 768 {
            let mut w = varint(1);
            write_str(&mut w, "minecraft:furnace_input");
            w.extend([varint(1), varint(old("stone"))].concat());
            w.extend([varint(1), varint(2), varint(old("stone"))].concat());
            w.extend([slot(p, "item_stack"), stack(p, old("stone_slab"), 2)].concat());
            w
        } else {
            let mut w = varint(1);
            write_str(&mut w, "vanillite:stonecutter/0");
            w.extend(varint(19));
            write_str(&mut w, "");
            w.extend([varint(1), varint(1), varint(old("stone")), vec![0, 0]].concat());
            w.extend([varint(2), varint(old("stone_slab")), vec![0, 0]].concat());
            w
        };
        assert_eq!(got, Some(ids.old_cb("update_recipes", &want)), "{dir}");
    }
}

/// Advancements reach every older version: the position back in the display, the icon a
/// stack before 26.1, no screen flag before 1.21.5.
#[test]
fn advancements_down_every_step() {
    let title = [8u8, 0, 1, b'a'];
    for dir in ALL {
        let ids = Ids::load(dir);
        let p = ids.protocol;
        let (_, mut c) = ids.halves();
        let pos = [1.0f32.to_be_bytes(), 2.0f32.to_be_bytes()].concat();
        let mut new = vec![1];
        new.extend(varint(1));
        write_str(&mut new, "minecraft:story/root");
        new.extend([0, 1]);
        new.extend([title, title].concat());
        new.extend(stack(775, ids.new_id("minecraft:item", "stick"), 1));
        new.extend(varint(0));
        new.extend(2i32.to_be_bytes());
        new.extend([varint(0), vec![0]].concat());
        new.extend(&pos);
        new.extend([varint(0), varint(0), vec![1]].concat());
        let got = cb(&mut c, &ids.new_cb("update_advancements", &new));
        let mut want = vec![1];
        want.extend(varint(1));
        write_str(&mut want, "minecraft:story/root");
        want.extend([0, 1]);
        want.extend([title, title].concat());
        want.extend(stack(p, ids.old_id("minecraft:item", "stick"), 1));
        want.extend(varint(0));
        want.extend(2i32.to_be_bytes());
        want.extend(&pos);
        want.extend([varint(0), vec![0]].concat());
        want.extend([varint(0), varint(0)].concat());
        if p >= 770 {
            want.push(1);
        }
        assert_eq!(got, Some(ids.old_cb("update_advancements", &want)), "{dir}");
    }
}

/// Shulker box contents and written books reach every older version (stack templates from
/// 26.1 on, stacks before).
#[test]
fn container_and_book_items_down_every_step() {
    let mut book = Vec::new();
    write_str(&mut book, "t");
    book.push(0);
    write_str(&mut book, "a");
    book.extend([0, 1, 8, 0, 1, b'p', 0, 1]);
    for dir in ALL {
        let ids = Ids::load(dir);
        let p = ids.protocol;
        let (_, mut c) = ids.halves();
        let component = |new: bool, name: &str| {
            varint(if new {
                ids.new_id("minecraft:data_component_type", name)
            } else {
                ids.old_id("minecraft:data_component_type", name)
            })
        };
        let item = |new: bool, name: &str| {
            if new {
                ids.new_id("minecraft:item", name)
            } else {
                ids.old_id("minecraft:item", name)
            }
        };
        let slot = |new: bool, stack: Vec<u8>| {
            let fields = [vec![0, 0], 36i16.to_be_bytes().to_vec(), stack].concat();
            if new {
                ids.new_cb("container_set_slot", &fields)
            } else {
                ids.old_cb("container_set_slot", &fields)
            }
        };
        let shulker = |new: bool| {
            let pr = if new { 777 } else { p };
            [
                varint(1),
                varint(item(new, "shulker_box")),
                vec![1, 0],
                component(new, "container"),
                vec![1],
                if pr >= 775 { vec![1] } else { vec![] },
                stack(pr, item(new, "stick"), 1),
            ]
            .concat()
        };
        assert_eq!(
            cb(&mut c, &slot(true, shulker(true))),
            Some(slot(false, shulker(false))),
            "container for {dir}"
        );
        let written = |new: bool| {
            [
                varint(1),
                varint(item(new, "written_book")),
                vec![1, 0],
                component(new, "written_book_content"),
                book.clone(),
            ]
            .concat()
        };
        assert_eq!(
            cb(&mut c, &slot(true, written(true))),
            Some(slot(false, written(false))),
            "written book for {dir}"
        );
    }
}
