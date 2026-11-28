#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use super::*;
use crate::components::TYPE_COUNT;

/// Static registries plus dynamic ones whose entries get ids as they are first named.
#[derive(Default)]
struct TestNames {
    entries: RefCell<HashMap<String, Vec<&'static str>>>,
}

impl Registries for TestNames {
    fn size(&self, _registry: &str) -> Option<usize> {
        Some(1 << 20)
    }
}

impl RegistryNames for TestNames {
    fn entry_name(&self, registry: &str, id: usize) -> Option<&str> {
        self.entries.borrow().get(registry)?.get(id).copied()
    }

    fn entry_id(&self, registry: &str, name: &str) -> Option<usize> {
        let mut entries = self.entries.borrow_mut();
        let list = entries.entry(registry.to_owned()).or_default();
        if let Some(i) = list.iter().position(|n| *n == name) {
            return Some(i);
        }
        list.push(Box::leak(name.to_owned().into_boxed_str()));
        Some(list.len() - 1)
    }
}

fn from_json(v: &serde_json::Value) -> Tag {
    use serde_json::Value;
    match v {
        Value::Null => Tag::Compound(Compound::new()),
        Value::Bool(b) => Tag::Byte(i8::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i32::try_from(i).map_or(Tag::Long(i), Tag::Int),
            None => Tag::Double(n.as_f64().unwrap()),
        },
        Value::String(s) => Tag::String(s.clone()),
        Value::Array(a) => Tag::List(List(a.iter().map(from_json).collect())),
        Value::Object(o) => {
            let mut c = Compound::new();
            for (k, v) in o {
                c.insert(k.clone(), from_json(v));
            }
            Tag::Compound(c)
        }
    }
}

/// Equality of two persistent values regardless of number tag types, array vs list tags
/// and key order (JSON and NBT forms of the same value compare equal).
fn equivalent(a: &Tag, b: &Tag) -> bool {
    fn num(t: &Tag) -> Option<f64> {
        match *t {
            Tag::Byte(v) => Some(f64::from(v)),
            Tag::Short(v) => Some(f64::from(v)),
            Tag::Int(v) => Some(f64::from(v)),
            Tag::Long(v) => Some(v as f64),
            Tag::Float(v) => Some(f64::from(v)),
            Tag::Double(v) => Some(v),
            _ => None,
        }
    }
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return (x - y).abs() <= 1e-6 * x.abs().max(1.0);
    }
    match (elements(a), elements(b)) {
        (Ok(x), Ok(y)) => {
            return x.len() == y.len() && x.iter().zip(&y).all(|(a, b)| equivalent(a, b));
        }
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => return false,
        _ => {}
    }
    match (a, b) {
        (Tag::Compound(x), Tag::Compound(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| equivalent(v, w)))
        }
        _ => a == b,
    }
}

/// Converts a persistent value to the wire and back: the result must be equivalent and
/// convert to the same bytes again.
fn round_trip(names: &TestNames, type_name: &str, value: &Tag) -> std::result::Result<(), String> {
    let t = type_by_name(type_name).ok_or(format!("{type_name}: unknown type"))?;
    let wire = from_nbt(t, value, names)
        .map_err(|e| format!("{type_name}: {value:?} does not convert: {e:?}"))?;
    let back = to_nbt(t, &wire, names)
        .map_err(|e| format!("{type_name}: wire form does not convert back: {e:?}"))?;
    if !equivalent(value, &back) {
        return Err(format!("{type_name}:\n  in:  {value:?}\n  out: {back:?}"));
    }
    let again = from_nbt(t, &back, names).map_err(|e| format!("{type_name}: {e:?}"))?;
    if wire != again {
        return Err(format!("{type_name}: wire form not stable"));
    }
    Ok(())
}

#[test]
fn vanilla_default_components_round_trip() {
    let items: serde_json::Value =
        serde_json::from_str(vanillite_data::raw::ITEMS_JSON).expect("items.json");
    let names = TestNames::default();
    let mut seen = 0;
    let mut failures = Vec::new();
    for item in items.as_array().unwrap() {
        for (name, value) in item["components"].as_object().unwrap() {
            if let Err(e) = round_trip(&names, name, &from_json(value)) {
                failures.push(e);
            }
            seen += 1;
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(seen > 20_000, "{seen}");
}

/// Persistent values of every type that the default components do not show, in Vanilla's
/// form.
const SAMPLES: &[(&str, &str)] = &[
    ("unbreakable", "{}"),
    ("glider", "{}"),
    ("waxed", "{}"),
    ("intangible_projectile", "{}"),
    ("custom_name", "{text:\"Hi\",color:\"red\",italic:0b}"),
    ("custom_name", "\"plain\""),
    ("lore", "[\"a\",{translate:\"b\",with:[1]}]"),
    ("dyed_color", "16711680"),
    ("map_id", "42"),
    ("damage", "17"),
    ("tooltip_style", "\"minecraft:fancy\""),
    ("note_block_sound", "\"minecraft:block.note_block.bell\""),
    ("rarity", "\"epic\""),
    (
        "enchantments",
        "{\"minecraft:sharpness\":5,\"minecraft:unbreaking\":3}",
    ),
    ("stored_enchantments", "{\"minecraft:mending\":1}"),
    ("custom_data", "{foo:{bar:[1,2,3]},s:\"x\"}"),
    (
        "custom_model_data",
        "{floats:[1.5f,2.0f],flags:[B;1b,0b],strings:[\"a\"],colors:[I;255,65280]}",
    ),
    (
        "tooltip_display",
        "{hide_tooltip:1b,hidden_components:[\"minecraft:enchantments\",\"minecraft:dyed_color\"]}",
    ),
    (
        "can_break",
        "{blocks:\"#minecraft:logs\",state:{axis:\"y\",age:{min:\"1\",max:\"3\"}},nbt:\"{a:1b}\",\
         components:{\"minecraft:damage\":3},predicates:{\"minecraft:damage\":{durability:5},\
         \"minecraft:custom_name\":{}}}",
    ),
    (
        "can_place_on",
        "[{blocks:\"minecraft:stone\"},{blocks:[\"minecraft:dirt\",\"minecraft:grass_block\"]}]",
    ),
    (
        "attribute_modifiers",
        "[{type:\"minecraft:attack_damage\",id:\"minecraft:base\",amount:5.0d,\
         operation:\"add_value\",slot:\"mainhand\"},{type:\"minecraft:armor\",id:\"x:y\",\
         amount:0.5d,operation:\"add_multiplied_total\",display:{type:\"override\",value:\"z\"}},\
         {type:\"minecraft:armor\",id:\"x:z\",amount:1.0d,operation:\"add_value\",\
         display:{type:\"hidden\"}}]",
    ),
    (
        "consumable",
        "{consume_seconds:2.0f,animation:\"drink\",sound:{sound_id:\"x:y\",range:8.0f},\
         has_consume_particles:0b,on_consume_effects:[{type:\"minecraft:apply_effects\",\
         effects:[{id:\"minecraft:speed\",amplifier:1b,duration:200,show_icon:1b}],\
         probability:0.5f},{type:\"minecraft:remove_effects\",effects:\"#minecraft:x\"},\
         {type:\"minecraft:clear_all_effects\"},{type:\"minecraft:teleport_randomly\",\
         diameter:8.0f},{type:\"minecraft:play_sound\",sound:\"minecraft:entity.player.burp\"}]}",
    ),
    (
        "death_protection",
        "{death_effects:[{type:\"minecraft:clear_all_effects\"}]}",
    ),
    (
        "potion_contents",
        "{potion:\"minecraft:healing\",custom_color:255,custom_effects:[{id:\"minecraft:speed\",\
         amplifier:-1b,duration:20,ambient:1b,show_particles:0b,show_icon:0b,\
         hidden_effect:{duration:600,show_icon:1b}}],custom_name:\"n\"}",
    ),
    (
        "suspicious_stew_effects",
        "[{id:\"minecraft:night_vision\"},{id:\"minecraft:saturation\",duration:7}]",
    ),
    (
        "writable_book_content",
        "{pages:[{raw:\"a\"},{raw:\"b\",filtered:\"c\"}]}",
    ),
    (
        "written_book_content",
        "{title:{raw:\"T\"},author:\"me\",generation:2,pages:[{raw:\"p\"},\
         {raw:{text:\"q\"},filtered:\"r\"}],resolved:1b}",
    ),
    (
        "trim",
        "{material:\"minecraft:gold\",pattern:{asset_id:\"x:y\",description:\"d\",decal:1b}}",
    ),
    (
        "provides_trim_material",
        "{palette_id:\"x:p\",description:\"m\"}",
    ),
    ("block_entity_data", "{id:\"minecraft:chest\",Items:[]}"),
    ("bucket_entity_data", "{Health:3.0f}"),
    (
        "instrument",
        "{sound_event:\"minecraft:item.goat_horn.sound.0\",use_duration:7.0f,range:256.0f,\
         description:\"x\"}",
    ),
    ("jukebox_playable", "\"minecraft:cat\""),
    (
        "provides_banner_patterns",
        "[\"minecraft:creeper\",\"minecraft:skull\"]",
    ),
    ("recipes", "[\"minecraft:stick\"]"),
    (
        "lodestone_tracker",
        "{target:{dimension:\"minecraft:overworld\",pos:[I;-5,70,123456]},tracked:0b}",
    ),
    ("lodestone_tracker", "{}"),
    (
        "firework_explosion",
        "{shape:\"creeper\",colors:[I;1,2],fade_colors:[I;3],has_trail:1b,has_twinkle:1b}",
    ),
    (
        "fireworks",
        "{flight_duration:3b,explosions:[{shape:\"star\"},{shape:\"burst\",colors:[I;5]}]}",
    ),
    ("profile", "{name:\"Notch\"}"),
    (
        "profile",
        "{id:[I;1,2,3,4],name:\"Steve\",properties:[{name:\"textures\",value:\"v\",\
         signature:\"s\"}],texture:\"x:t\",model:\"slim\"}",
    ),
    ("profile", "{id:[I;1,2,3,4],cape:\"x:c\",model:\"wide\"}"),
    (
        "banner_patterns",
        "[{pattern:\"minecraft:creeper\",color:\"red\"},{pattern:{asset_id:\"x:y\",\
         translation_key:\"k\"},color:\"black\"}]",
    ),
    ("base_color", "\"light_blue\""),
    (
        "pot_decorations",
        "{back:{id:\"minecraft:brick\"},front:{id:\"minecraft:angler_pottery_sherd\"}}",
    ),
    (
        "container",
        "[{slot:0,item:{id:\"minecraft:stone\",count:5}},{slot:3,item:{id:\"minecraft:diamond_sword\",\
         components:{\"minecraft:damage\":5,\"!minecraft:enchantments\":{}}}}]",
    ),
    (
        "bundle_contents",
        "[{id:\"minecraft:stone\",count:3},{id:\"minecraft:shulker_box\",components:{\
         \"minecraft:container\":[{slot:1,item:{id:\"minecraft:apple\"}}]}}]",
    ),
    ("charged_projectiles", "[{id:\"minecraft:arrow\"}]"),
    ("use_remainder", "{id:\"minecraft:bowl\"}"),
    (
        "sulfur_cube_content",
        "{id:\"minecraft:slime_block\",count:2}",
    ),
    (
        "bees",
        "[{entity_data:{id:\"minecraft:bee\",Health:5.0f},ticks_in_hive:3,min_ticks_in_hive:600}]",
    ),
    ("lock", "{items:\"minecraft:stick\"}"),
    (
        "container_loot",
        "{loot_table:\"minecraft:chests/simple_dungeon\",seed:42L}",
    ),
    (
        "map_decorations",
        "{a:{type:\"minecraft:red_x\",x:1.0d,z:2.0d,rotation:180.0f}}",
    ),
    ("debug_stick_state", "{\"minecraft:oak_log\":\"axis\"}"),
    ("compostable", "{layers:\"minecraft:some_provider\"}"),
    ("cooking_fuel", "{burn_time:200,speed_multiplier:\"x:y\"}"),
    (
        "sign_text_back",
        "{messages:[\"a\",\"b\",\"c\",\"d\"],filtered_messages:[\"a\",\"\",\"\",\"\"],\
         color:\"red\",has_glowing_text:1b}",
    ),
    ("wolf/variant", "\"minecraft:pale\""),
    ("wolf/sound_variant", "\"minecraft:classic\""),
    ("wolf/collar", "\"red\""),
    ("fox/variant", "\"snow\""),
    ("salmon/size", "\"large\""),
    ("parrot/variant", "\"gray\""),
    ("tropical_fish/pattern", "\"clayfish\""),
    ("tropical_fish/base_color", "\"lime\""),
    ("tropical_fish/pattern_color", "\"pink\""),
    ("mooshroom/variant", "\"brown\""),
    ("rabbit/variant", "\"evil\""),
    ("pig/variant", "\"minecraft:cold\""),
    ("pig/sound_variant", "\"minecraft:big\""),
    ("cow/variant", "\"minecraft:warm\""),
    ("cow/sound_variant", "\"minecraft:moody\""),
    ("chicken/sound_variant", "\"minecraft:picky\""),
    ("zombie_nautilus/variant", "\"minecraft:warm\""),
    ("frog/variant", "\"minecraft:cold\""),
    ("horse/variant", "\"dark_brown\""),
    ("painting/variant", "\"minecraft:kebab\""),
    ("llama/variant", "\"gray\""),
    ("axolotl/variant", "\"blue\""),
    ("cat/variant", "\"minecraft:tabby\""),
    ("cat/sound_variant", "\"minecraft:classic\""),
    ("cat/collar", "\"black\""),
    ("sheep/color", "\"white\""),
    ("shulker/color", "\"purple\""),
    ("villager/variant", "\"minecraft:desert\""),
    ("brewing_fuel", "{uses:20,speed_multiplier:1.5f}"),
    ("ominous_bottle_amplifier", "4"),
];

#[test]
fn samples_round_trip_and_every_type_is_covered() {
    let names = TestNames::default();
    let mut covered = BTreeSet::new();
    let mut failures = Vec::new();
    for (name, snbt) in SAMPLES {
        let tag = vanillite_nbt::snbt::parse(snbt)
            .unwrap_or_else(|e| panic!("{name}: bad sample: {e:?}"));
        if let Err(e) = round_trip(&names, &format!("minecraft:{name}"), &tag) {
            failures.push(e);
        }
        covered.insert(format!("minecraft:{name}"));
    }
    let items: serde_json::Value = serde_json::from_str(vanillite_data::raw::ITEMS_JSON).unwrap();
    for item in items.as_array().unwrap() {
        for name in item["components"].as_object().unwrap().keys() {
            covered.insert(name.clone());
        }
    }
    let mut missing = Vec::new();
    for t in 0..TYPE_COUNT {
        let t = u16::try_from(t).unwrap();
        let name = type_name(t).unwrap();
        if !is_transient(t) && !covered.contains(name) {
            missing.push(name);
        }
        assert_eq!(
            is_transient(t),
            schema(name.strip_prefix("minecraft:").unwrap()).is_none(),
            "{name}"
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(missing.is_empty(), "types without a sample: {missing:?}");
}

/// The forms players write in `/give` item arguments (#251), including Vanilla's shorthands.
#[test]
fn command_shorthands_decode() {
    let names = TestNames::default();
    for (name, snbt) in [
        ("potion_contents", r#"{potion:"minecraft:swiftness"}"#),
        ("potion_contents", r#""minecraft:swiftness""#),
        ("potion_contents", r#""swiftness""#),
        (
            "written_book_content",
            r#"{title:"T",author:"A",pages:["hello"]}"#,
        ),
        (
            "written_book_content",
            r#"{title:{raw:"T"},author:"A",pages:[{raw:"a",filtered:"b"},{text:"c"}]}"#,
        ),
        ("writable_book_content", r#"{pages:["a",{raw:"b"}]}"#),
        ("dyed_color", "16711680"),
        ("dyed_color", "[1.0f,0.0f,0.5f]"),
        ("profile", r#"{name:"Notch"}"#),
        ("profile", r#""Notch""#),
        (
            "fireworks",
            r#"{flight_duration:2,explosions:[{shape:"star",colors:[I;255]}]}"#,
        ),
        ("fireworks", r#"{flight_duration:2b}"#),
        (
            "firework_explosion",
            r#"{shape:"creeper",colors:[I;1,2],has_trail:true}"#,
        ),
        ("custom_model_data", r#"{colors:[1,[0.0f,1.0f,0.0f]]}"#),
        ("enchantments", "{sharpness:5}"),
        ("custom_name", r#""Blade""#),
        ("damage", "10"),
    ] {
        let t = type_by_name(&format!("minecraft:{name}")).unwrap();
        let tag = vanillite_nbt::snbt::parse(snbt).unwrap();
        if let Err(e) = from_nbt(t, &tag, &names) {
            panic!("{name}={snbt}: {e}");
        }
    }
    let dyed = type_by_name("minecraft:dyed_color").unwrap();
    let rgb = vanillite_nbt::snbt::parse("[1.0f,0.0f,0.5f]").unwrap();
    assert_eq!(
        &*from_nbt(dyed, &rgb, &names).unwrap(),
        &0xFFFF_007F_u32.to_be_bytes()
    );
    for (name, snbt) in [
        ("dyed_color", "[1.0f,0.0f]"),
        ("potion_contents", "5"),
        ("written_book_content", r#"{author:"A"}"#),
        ("fireworks", r#"{explosions:[{shape:"square"}]}"#),
    ] {
        let t = type_by_name(&format!("minecraft:{name}")).unwrap();
        let tag = vanillite_nbt::snbt::parse(snbt).unwrap();
        assert!(from_nbt(t, &tag, &names).is_err(), "{name}: {snbt}");
    }
}

#[test]
fn defaults_are_omitted_like_vanilla() {
    let names = TestNames::default();
    let t = type_by_name("minecraft:tool").unwrap();
    let mut wire = Vec::new();
    write_len(&mut wire, 0); // no rules
    wire.extend_from_slice(&1.0f32.to_be_bytes());
    write_varint(&mut wire, 1);
    wire.push(1);
    let tag = to_nbt(t, &wire, &names).unwrap();
    let expected = vanillite_nbt::snbt::parse("{rules:[]}").unwrap();
    assert_eq!(tag, expected);
}

#[test]
fn inline_values_without_persistent_form_fail() {
    let names = TestNames::default();
    let t = type_by_name("minecraft:painting/variant").unwrap();
    let mut wire = vec![0, 1, 1];
    write_str(&mut wire, "minecraft:kebab");
    wire.extend([0, 0]);
    assert!(to_nbt(t, &wire, &names).is_err());
    let t = type_by_name("minecraft:creative_slot_lock").unwrap();
    assert!(to_nbt(t, &[], &names).is_err());
}

#[test]
fn bad_nbt_is_rejected() {
    let names = TestNames::default();
    for (name, snbt) in [
        ("damage", "\"x\""),
        ("rarity", "\"legendary\""),
        ("enchantments", "{\"minecraft:sharpness\":300}"),
        ("food", "{saturation:1.0f}"),
        ("container", "[{slot:300,item:{id:\"minecraft:stone\"}}]"),
        (
            "lodestone_tracker",
            "{target:{dimension:\"minecraft:overworld\",pos:[I;1,2]}}",
        ),
    ] {
        let t = type_by_name(&format!("minecraft:{name}")).unwrap();
        let tag = vanillite_nbt::snbt::parse(snbt).unwrap();
        assert!(from_nbt(t, &tag, &names).is_err(), "{name}: {snbt}");
    }
}

#[test]
fn block_positions_pack_like_vanilla() {
    for (x, y, z) in [
        (0, 0, 0),
        (-1, -64, -1),
        (30_000_000, 319, -30_000_000),
        (5, -2048, 7),
    ] {
        assert_eq!(unpack_pos(pack_pos(x, y, z)), [x, y, z]);
    }
}

/// `CustomData.COMPOUND_TAG_CODEC` (I6-02 of #272): `custom_data`, `bucket_entity_data` and the
/// typed `entity_data` also read an SNBT string; `lock` does not.
#[test]
fn snbt_strings_for_compound_components() {
    let names = TestNames::default();
    let id = |n: &str| type_by_name(&format!("minecraft:{n}")).unwrap();
    let snbt = Tag::String("{quest:1b,list:[1,2]}".into());
    let wire = from_nbt(id("custom_data"), &snbt, &names).unwrap();
    let back = to_nbt(id("custom_data"), &wire, &names).unwrap();
    assert_eq!(
        back,
        Tag::Compound(vanillite_nbt::snbt::parse_compound("{quest:1b,list:[1,2]}").unwrap())
    );
    assert!(from_nbt(id("bucket_entity_data"), &snbt, &names).is_ok());
    let typed = Tag::String("{id:\"minecraft:pig\",NoAI:1b}".into());
    let wire = from_nbt(id("entity_data"), &typed, &names).unwrap();
    let Tag::Compound(c) = to_nbt(id("entity_data"), &wire, &names).unwrap() else {
        panic!("entity_data is a compound");
    };
    assert_eq!(c.get("NoAI"), Some(&Tag::Byte(1)));
    assert!(from_nbt(id("lock"), &snbt, &names).is_err());
    assert!(from_nbt(id("custom_data"), &Tag::String("{bad".into()), &names).is_err());
}
