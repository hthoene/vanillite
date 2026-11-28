//! Item stacks as gameplay sees them: effective component values (patch over the item's
//! defaults), tools and enchantments.

pub use vanillite_protocol::item::{ComponentPatch, ItemStack};

use vanillite_data::block::{BLOCK_COUNT, BlockState};
use vanillite_data::item::{BlockSet, ItemId, TOOLS, ToolInfo};
use vanillite_protocol::codec::Reader;
use vanillite_protocol::components::ids;

/// `EquipmentSlot` in Vanilla order (its ordinal is the wire id of `set_equipment`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EquipmentSlot {
    MainHand = 0,
    OffHand = 1,
    Feet = 2,
    Legs = 3,
    Chest = 4,
    Head = 5,
    Body = 6,
    Saddle = 7,
}

impl EquipmentSlot {
    /// The slot of an ordinal.
    pub fn from_ordinal(v: u8) -> Option<EquipmentSlot> {
        Some(match v {
            0 => EquipmentSlot::MainHand,
            1 => EquipmentSlot::OffHand,
            2 => EquipmentSlot::Feet,
            3 => EquipmentSlot::Legs,
            4 => EquipmentSlot::Chest,
            5 => EquipmentSlot::Head,
            6 => EquipmentSlot::Body,
            7 => EquipmentSlot::Saddle,
            _ => return None,
        })
    }

    /// The slot of its stream id (`EquipmentSlot.STREAM_CODEC`: main hand 0, feet 1, legs 2,
    /// chest 3, head 4, off hand 5, body 6, saddle 7), as component values use it.
    pub fn from_id(v: u8) -> Option<EquipmentSlot> {
        Some(match v {
            0 => EquipmentSlot::MainHand,
            1 => EquipmentSlot::Feet,
            2 => EquipmentSlot::Legs,
            3 => EquipmentSlot::Chest,
            4 => EquipmentSlot::Head,
            5 => EquipmentSlot::OffHand,
            6 => EquipmentSlot::Body,
            7 => EquipmentSlot::Saddle,
            _ => return None,
        })
    }

    /// The ordinal (wire id).
    pub fn ordinal(self) -> u8 {
        #[allow(clippy::as_conversions)]
        let v = self as u8;
        v
    }

    /// Whether this is a humanoid armour slot.
    pub fn is_armor(self) -> bool {
        matches!(
            self,
            EquipmentSlot::Feet | EquipmentSlot::Legs | EquipmentSlot::Chest | EquipmentSlot::Head
        )
    }
}

/// Reads a VarInt component value.
fn varint_value(data: &[u8]) -> Option<i32> {
    let mut r = Reader::new(data);
    r.varint().ok()
}

/// The item of a stack.
#[inline]
pub fn item_id(stack: &ItemStack) -> ItemId {
    ItemId(stack.item)
}

/// `getMaxStackSize`: the `max_stack_size` component, patched or default.
pub fn max_stack_size(stack: &ItemStack) -> i32 {
    if let Some(p) = stack.patch() {
        if let Some(v) = p.get(ids::MAX_STACK_SIZE).and_then(varint_value) {
            return v;
        }
        if p.is_removed(ids::MAX_STACK_SIZE) {
            return 1;
        }
    }
    i32::from(item_id(stack).info().max_stack_size)
}

/// `max_damage`, 0 if the stack has none.
pub fn max_damage(stack: &ItemStack) -> i32 {
    if let Some(p) = stack.patch() {
        if let Some(v) = p.get(ids::MAX_DAMAGE).and_then(varint_value) {
            return v;
        }
        if p.is_removed(ids::MAX_DAMAGE) {
            return 0;
        }
    }
    i32::from(item_id(stack).info().max_damage)
}

/// `getDamageValue`.
pub fn damage(stack: &ItemStack) -> i32 {
    stack
        .component(ids::DAMAGE)
        .and_then(varint_value)
        .unwrap_or(0)
        .clamp(0, max_damage(stack).max(0))
}

/// `isDamageableItem`: has `max_damage` and `damage` and is not unbreakable.
pub fn is_damageable(stack: &ItemStack) -> bool {
    max_damage(stack) > 0
        && stack.component(ids::UNBREAKABLE).is_none()
        && match patched(stack, ids::DAMAGE) {
            Patched::Value(_) => true,
            Patched::Removed => false,
            Patched::Default => item_id(stack).info().max_damage > 0,
        }
}

/// The damage type tag of the stack's `damage_resistant` component (`DamageResistant.types`,
/// e.g. `minecraft:is_fire`): the patched one, else the item's default.
pub fn damage_resistant(stack: &ItemStack) -> Option<&str> {
    if let Some(data) = stack.component(ids::DAMAGE_RESISTANT) {
        // `HolderSet` of damage types: 0 and the tag (a list of single types is not kept).
        let mut r = Reader::new(data);
        return match r.varint() {
            Ok(0) => r.string(32767).ok(),
            _ => None,
        };
    }
    if stack
        .patch()
        .is_some_and(|p| p.is_removed(ids::DAMAGE_RESISTANT))
    {
        return None;
    }
    let tag = item_id(stack).info().damage_resistant;
    (!tag.is_empty()).then_some(tag)
}

/// `ItemStack.canBeHurtBy(source)`: its `damage_resistant` component does not cover the
/// damage type `kind`.
pub fn can_be_hurt_by(
    stack: &ItemStack,
    types: &crate::survival::damage::DamageTypes,
    kind: crate::survival::damage::DamageKind,
) -> bool {
    let Some(resists) = damage_resistant(stack) else {
        return true;
    };
    crate::survival::damage::tag::NAMES
        .iter()
        .find(|(name, _)| *name == resists)
        .is_none_or(|&(_, bit)| !types.is(kind, bit))
}

/// `isStackable`.
pub fn is_stackable(stack: &ItemStack) -> bool {
    max_stack_size(stack) > 1 && (!is_damageable(stack) || damage(stack) == 0)
}

/// Sets the `damage` component.
pub fn set_damage(stack: &mut ItemStack, value: i32) {
    let mut bytes = Vec::with_capacity(5);
    vanillite_protocol::codec::write_varint(&mut bytes, value.max(0));
    stack.set_component(ids::DAMAGE, bytes.into());
}

/// The equipment slot a player puts the item in (`Player.getEquipmentSlotForItem`: the
/// `equippable` slot where players can use it, else the main hand).
pub fn equipment_slot(stack: &ItemStack) -> EquipmentSlot {
    equippable(stack)
        .map(|e| e.slot)
        .filter(|s| !matches!(s, EquipmentSlot::Body | EquipmentSlot::Saddle))
        .unwrap_or(EquipmentSlot::MainHand)
}

/// Where a component of a stack comes from.
#[derive(Debug, Clone, Copy)]
pub enum Patched<'a> {
    /// The patch sets this value.
    Value(&'a [u8]),
    /// The patch removes the item's default.
    Removed,
    /// The item's default applies.
    Default,
}

/// The patch state of component `type_id` of a stack.
pub fn patched(stack: &ItemStack, type_id: u16) -> Patched<'_> {
    match stack.patch() {
        Some(p) => match p.get(type_id) {
            Some(v) => Patched::Value(v),
            None if p.is_removed(type_id) => Patched::Removed,
            None => Patched::Default,
        },
        None => Patched::Default,
    }
}

/// The `equippable` component (`Equippable`).
#[derive(Debug, Clone)]
pub struct Equippable {
    pub slot: EquipmentSlot,
    pub equip_sound: crate::consumable::Sound,
    /// Entity types that can wear it (`None`: all).
    pub allowed_entities: Option<crate::consumable::HolderSet>,
    pub dispensable: bool,
    pub swappable: bool,
    pub damage_on_hurt: bool,
    pub equip_on_interact: bool,
    pub can_be_sheared: bool,
    pub shearing_sound: crate::consumable::Sound,
}

impl Equippable {
    /// `Equippable.STREAM_CODEC`.
    fn read(data: &[u8]) -> Option<Equippable> {
        use crate::consumable::{read_bool, read_holder_set};
        let mut r = Reader::new(data);
        let slot = EquipmentSlot::from_id(u8::try_from(r.varint().ok()?).ok()?)?;
        let equip_sound = crate::consumable::read_sound(&mut r)?;
        for _ in 0..2 {
            if read_bool(&mut r)? {
                r.string(32767).ok()?;
            }
        }
        let allowed_entities = if read_bool(&mut r)? {
            Some(read_holder_set(&mut r)?)
        } else {
            None
        };
        Some(Equippable {
            slot,
            equip_sound,
            allowed_entities,
            dispensable: read_bool(&mut r)?,
            swappable: read_bool(&mut r)?,
            damage_on_hurt: read_bool(&mut r)?,
            equip_on_interact: read_bool(&mut r)?,
            can_be_sheared: read_bool(&mut r)?,
            shearing_sound: crate::consumable::read_sound(&mut r)?,
        })
    }

    /// `canBeEquippedBy(type)`.
    pub fn can_be_equipped_by(&self, entity_type: u16) -> bool {
        match &self.allowed_entities {
            None => true,
            Some(crate::consumable::HolderSet::Ids(ids)) => ids.contains(&entity_type),
            Some(crate::consumable::HolderSet::Tag(tag)) => crate::player::play::game_data()
                .is_some_and(|d| {
                    d.entity_tags
                        .get(tag.as_str())
                        .is_some_and(|v| v.contains(&entity_type))
                }),
        }
    }
}

/// The stack's `equippable` component (patched or the item's default).
pub fn equippable(stack: &ItemStack) -> Option<Equippable> {
    if stack.is_empty() {
        return None;
    }
    match patched(stack, ids::EQUIPPABLE) {
        Patched::Value(v) => Equippable::read(v),
        Patched::Removed => None,
        Patched::Default => crate::item_nbt::default_component(stack.item, ids::EQUIPPABLE)
            .and_then(|v| Equippable::read(&v)),
    }
}

/// The `equippable` slot of a stack, if it has one.
pub fn equippable_slot(stack: &ItemStack) -> Option<EquipmentSlot> {
    match patched(stack, ids::EQUIPPABLE) {
        Patched::Value(_) => equippable(stack).map(|e| e.slot),
        Patched::Removed => None,
        Patched::Default if stack.is_empty() => None,
        Patched::Default => EquipmentSlot::from_ordinal(item_id(stack).info().equip_slot)
            .or_else(|| equippable(stack).map(|e| e.slot)),
    }
}

/// The stack's `weapon` component (`Weapon`).
pub fn weapon(stack: &ItemStack) -> Option<vanillite_data::item::WeaponInfo> {
    if stack.is_empty() {
        return None;
    }
    match patched(stack, ids::WEAPON) {
        Patched::Value(v) => {
            let mut r = Reader::new(v);
            Some(vanillite_data::item::WeaponInfo {
                item_damage_per_attack: u32::try_from(r.varint().ok()?).ok()?,
                disable_blocking_for_seconds: r.f32().ok()?,
            })
        }
        Patched::Removed => None,
        Patched::Default => item_id(stack).info().weapon,
    }
}

/// The damage type name of the stack's `damage_type` component (`ItemStack.getDamageSource`).
pub fn damage_type(stack: &ItemStack) -> Option<&'static str> {
    if stack.is_empty() {
        return None;
    }
    match patched(stack, ids::DAMAGE_TYPE) {
        Patched::Value(v) => {
            let id = usize::try_from(Reader::new(v).varint().ok()?).ok()?;
            vanillite_protocol::item::registry_entry_name("minecraft:damage_type", id)
        }
        Patched::Removed => None,
        Patched::Default => Some(item_id(stack).info().damage_type).filter(|t| !t.is_empty()),
    }
}

/// The stack's `attack_animation` (`getAttackAnimation`): the `SwingAnimationType` id (1
/// whack, 2 stab) and the duration; `SwingAnimation.DEFAULT` (whack, 6) without one. Only
/// the spears (`piercing_weapon`) have a default one: a stab.
pub fn attack_animation(stack: &ItemStack) -> (i32, i32) {
    if stack.is_empty() {
        return (1, 6);
    }
    match patched(stack, ids::ATTACK_ANIMATION) {
        Patched::Value(v) => {
            let mut r = Reader::new(v);
            let kind = r.varint().unwrap_or(1);
            (kind, r.varint().unwrap_or(6))
        }
        Patched::Removed => (1, 6),
        Patched::Default => {
            let info = item_id(stack).info();
            let kind = if info.piercing_weapon.is_some() { 2 } else { 1 };
            (kind, i32::from(info.attack_animation_ticks))
        }
    }
}

/// The ticks of the stack's `attack_animation` (`SwingAnimation.duration`; 6 without one).
pub fn attack_animation_ticks(stack: &ItemStack) -> i32 {
    if stack.is_empty() {
        return 6;
    }
    match patched(stack, ids::ATTACK_ANIMATION) {
        Patched::Value(v) => {
            let mut r = Reader::new(v);
            r.varint().ok().and_then(|_| r.varint().ok()).unwrap_or(6)
        }
        Patched::Removed => 6,
        Patched::Default => i32::from(item_id(stack).info().attack_animation_ticks),
    }
}

/// The stack's `enchantable` value, if it has the component; `None` for the item's default.
pub fn patched_enchantable(stack: &ItemStack) -> Option<Option<i32>> {
    match patched(stack, ids::ENCHANTABLE) {
        Patched::Value(v) => Some(varint_value(v)),
        Patched::Removed => Some(None),
        Patched::Default => None,
    }
}

/// The stack's patched `repairable` item set; `None` for the item's default.
pub fn patched_repairable(stack: &ItemStack) -> Option<Option<crate::consumable::HolderSet>> {
    match patched(stack, ids::REPAIRABLE) {
        Patched::Value(v) => Some(crate::consumable::read_holder_set(&mut Reader::new(v))),
        Patched::Removed => Some(None),
        Patched::Default => None,
    }
}

/// Whether the stack has the `glider` component.
pub fn has_glider(stack: &ItemStack) -> bool {
    match patched(stack, ids::GLIDER) {
        Patched::Value(_) => true,
        Patched::Removed => false,
        Patched::Default => {
            !stack.is_empty()
                && crate::item_nbt::default_component(stack.item, ids::GLIDER).is_some()
        }
    }
}

/// The `block_transformer` of the stack (`Item.useOn` of axes, shovels, hoes …).
pub fn block_transformer(stack: &ItemStack) -> Option<&'static str> {
    if stack.is_empty() {
        return None;
    }
    match patched(stack, ids::BLOCK_TRANSFORMER) {
        Patched::Value(v) => {
            let id = usize::try_from(Reader::new(v).varint().ok()?).ok()?;
            vanillite_protocol::item::registry_entry_name("minecraft:block_transformer", id)
        }
        Patched::Removed => None,
        Patched::Default => Some(item_id(stack).info().block_transformer).filter(|t| !t.is_empty()),
    }
}

/// Enchantments of a stack: `(minecraft:enchantment id, level)`.
pub fn enchantments(stack: &ItemStack) -> Vec<(u32, i32)> {
    enchantment_list(stack, ids::ENCHANTMENTS)
}

/// The stored enchantments of a stack (enchanted books), like [`enchantments`].
pub fn stored_enchantments(stack: &ItemStack) -> Vec<(u32, i32)> {
    enchantment_list(stack, ids::STORED_ENCHANTMENTS)
}

fn enchantment_list(stack: &ItemStack, component: u16) -> Vec<(u32, i32)> {
    let Some(data) = stack.component(component) else {
        return Vec::new();
    };
    let mut r = Reader::new(data);
    let mut out = Vec::new();
    let Ok(n) = r.varint() else {
        return out;
    };
    for _ in 0..n.clamp(0, 256) {
        let (Ok(id), Ok(level)) = (r.varint(), r.varint()) else {
            break;
        };
        if let Ok(id) = u32::try_from(id) {
            out.push((id, level));
        }
    }
    out
}

/// A tool rule with its block set resolved to a bit set over block ids.
#[derive(Debug, Clone)]
struct ResolvedRule {
    blocks: Box<[u64]>,
    speed: Option<f32>,
    correct_for_drops: Option<bool>,
}

impl ResolvedRule {
    fn contains(&self, state: BlockState) -> bool {
        let b = usize::from(state.block().0);
        self.blocks
            .get(b / 64)
            .is_some_and(|w| w & (1u64 << (b % 64)) != 0)
    }
}

/// The default `minecraft:tool` components with their block tags resolved (built once at
/// start from the loaded tags), and the block tags for tool components a stack's patch sets.
#[derive(Debug, Clone, Default)]
pub struct ToolTable {
    tools: Vec<Vec<ResolvedRule>>,
    /// Block tags by id (sorted block ids), for patched tool rules.
    tags: std::sync::Arc<std::collections::HashMap<String, Vec<u16>>>,
}

/// A `minecraft:tool` component set by a stack's patch (`Tool.STREAM_CODEC`).
struct PatchedTool {
    rules: Vec<(crate::consumable::HolderSet, Option<f32>, Option<bool>)>,
    default_mining_speed: f32,
    damage_per_block: u32,
    can_destroy_blocks_in_creative: bool,
}

impl PatchedTool {
    fn read(data: &[u8]) -> Option<PatchedTool> {
        let mut r = Reader::new(data);
        let n = usize::try_from(r.varint().ok()?).ok()?;
        let mut rules = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let blocks = crate::consumable::read_holder_set(&mut r)?;
            let speed = if crate::consumable::read_bool(&mut r)? {
                Some(r.f32().ok()?)
            } else {
                None
            };
            let correct = if crate::consumable::read_bool(&mut r)? {
                Some(crate::consumable::read_bool(&mut r)?)
            } else {
                None
            };
            rules.push((blocks, speed, correct));
        }
        let default_mining_speed = r.f32().ok()?;
        let damage_per_block = u32::try_from(r.varint().ok()?.max(0)).ok()?;
        let can_destroy_blocks_in_creative = crate::consumable::read_bool(&mut r)?;
        Some(PatchedTool {
            rules,
            default_mining_speed,
            damage_per_block,
            can_destroy_blocks_in_creative,
        })
    }
}

/// The tool component a stack uses.
enum ToolRef<'a> {
    Default(&'static ToolInfo, &'a [ResolvedRule]),
    Patched(PatchedTool),
}

impl ToolTable {
    /// Resolves every tool rule; `tag` returns the block ids of a block tag.
    pub fn new(tag: &dyn Fn(&str) -> Vec<u16>) -> ToolTable {
        let words = BLOCK_COUNT.div_ceil(64);
        let tools = TOOLS
            .iter()
            .map(|tool| {
                tool.rules
                    .iter()
                    .map(|rule| {
                        let mut bits = vec![0u64; words];
                        let ids = match rule.blocks {
                            BlockSet::Tag(t) => tag(t),
                            BlockSet::Blocks(b) => b.to_vec(),
                        };
                        for id in ids {
                            let i = usize::from(id);
                            if let Some(w) = bits.get_mut(i / 64) {
                                *w |= 1u64 << (i % 64);
                            }
                        }
                        ResolvedRule {
                            blocks: bits.into(),
                            speed: rule.speed,
                            correct_for_drops: rule.correct_for_drops,
                        }
                    })
                    .collect()
            })
            .collect();
        ToolTable {
            tools,
            tags: std::sync::Arc::default(),
        }
    }

    /// Sets the block tags (sorted block ids by tag id) patched tool rules are resolved with.
    pub fn set_block_tags(
        &mut self,
        tags: std::sync::Arc<std::collections::HashMap<String, Vec<u16>>>,
    ) {
        self.tags = tags;
    }

    fn tool(&self, stack: &ItemStack) -> Option<ToolRef<'_>> {
        if stack.is_empty() {
            return None;
        }
        if let Some(patch) = stack.patch() {
            if patch.is_removed(ids::TOOL) {
                return None;
            }
            if let Some(data) = patch.get(ids::TOOL) {
                return PatchedTool::read(data).map(ToolRef::Patched);
            }
        }
        let index = usize::from(item_id(stack).info().tool);
        Some(ToolRef::Default(TOOLS.get(index)?, self.tools.get(index)?))
    }

    /// Whether a patched rule's block set holds `state`.
    fn patched_contains(&self, blocks: &crate::consumable::HolderSet, state: BlockState) -> bool {
        let block = state.block().0;
        match blocks {
            crate::consumable::HolderSet::Tag(t) => self
                .tags
                .get(t.as_str())
                .is_some_and(|v| v.binary_search(&block).is_ok()),
            crate::consumable::HolderSet::Ids(ids) => ids.contains(&block),
        }
    }

    /// `ItemStack.getDestroySpeed`.
    pub fn destroy_speed(&self, stack: &ItemStack, state: BlockState) -> f32 {
        match self.tool(stack) {
            Some(ToolRef::Default(info, rules)) => rules
                .iter()
                .find_map(|r| r.speed.filter(|_| r.contains(state)))
                .unwrap_or(info.default_mining_speed),
            Some(ToolRef::Patched(tool)) => tool
                .rules
                .iter()
                .find_map(|(b, speed, _)| speed.filter(|_| self.patched_contains(b, state)))
                .unwrap_or(tool.default_mining_speed),
            None => 1.0,
        }
    }

    /// `ItemStack.isCorrectToolForDrops`.
    pub fn is_correct_for_drops(&self, stack: &ItemStack, state: BlockState) -> bool {
        match self.tool(stack) {
            Some(ToolRef::Default(_, rules)) => rules
                .iter()
                .find_map(|r| r.correct_for_drops.filter(|_| r.contains(state)))
                .unwrap_or(false),
            Some(ToolRef::Patched(tool)) => tool
                .rules
                .iter()
                .find_map(|(b, _, correct)| correct.filter(|_| self.patched_contains(b, state)))
                .unwrap_or(false),
            None => false,
        }
    }

    /// Whether the stack has a `minecraft:tool` component.
    pub fn is_tool(&self, stack: &ItemStack) -> bool {
        self.tool(stack).is_some()
    }

    /// `Tool.damagePerBlock`, 0 without a tool.
    pub fn damage_per_block(&self, stack: &ItemStack) -> u32 {
        match self.tool(stack) {
            Some(ToolRef::Default(info, _)) => info.damage_per_block,
            Some(ToolRef::Patched(tool)) => tool.damage_per_block,
            None => 0,
        }
    }

    /// `Item.canDestroyBlock`: tools that cannot destroy blocks in creative (swords, maces,
    /// tridents) cannot.
    pub fn can_destroy_in_creative(&self, stack: &ItemStack) -> bool {
        match self.tool(stack) {
            Some(ToolRef::Default(info, _)) => info.can_destroy_blocks_in_creative,
            Some(ToolRef::Patched(tool)) => tool.can_destroy_blocks_in_creative,
            None => true,
        }
    }
}

/// `ItemStack.matches`: same item, count and components.
#[inline]
pub fn matches(a: &ItemStack, b: &ItemStack) -> bool {
    (a.is_empty() && b.is_empty()) || (a.count == b.count && a.same_item_same_components(b))
}

/// The attribute id of `name` (index into `ATTRIBUTES`).
pub fn attribute_id(name: &str) -> Option<u16> {
    vanillite_data::entity::ATTRIBUTES
        .iter()
        .position(|a| a.name == name)
        .and_then(|i| u16::try_from(i).ok())
}

/// One entry of a patched `attribute_modifiers` component.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchedModifier {
    pub attribute: u16,
    pub id: String,
    pub amount: f64,
    pub operation: u8,
    pub slot_group: u8,
}

/// `ItemAttributeModifiers.STREAM_CODEC`.
pub fn read_attribute_modifiers(data: &[u8]) -> Option<Vec<PatchedModifier>> {
    let mut r = Reader::new(data);
    let n = usize::try_from(r.varint().ok()?).ok()?;
    let mut out = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let attribute = u16::try_from(r.varint().ok()?).ok()?;
        let id = r.string(32767).ok()?.to_owned();
        let amount = r.f64().ok()?;
        let operation = u8::try_from(r.varint().ok()?).ok()?;
        let slot_group = u8::try_from(r.varint().ok()?).ok()?;
        // `ItemAttributeModifiers.Display`: an override carries its text.
        if r.varint().ok()? == 2 {
            let mut rest = r.rest();
            let before = rest.len();
            let mut budget = vanillite_nbt::read::Budget::new(2 * 1024 * 1024, 512);
            vanillite_nbt::read::read_network(&mut rest, &mut budget).ok()?;
            r.take(before.checked_sub(rest.len())?).ok()?;
        }
        out.push(PatchedModifier {
            attribute,
            id,
            amount,
            operation,
            slot_group,
        });
    }
    Some(out)
}

/// The `attribute_modifiers` of a stack for `attribute` while worn in `slot`, as
/// `(operation, amount)`, followed by those of its enchantments (`minecraft:attributes`).
pub fn attribute_modifiers(
    stack: &ItemStack,
    slot: EquipmentSlot,
    attribute: &str,
    out: &mut Vec<(u8, f64)>,
) {
    if stack.is_empty() {
        return;
    }
    if let Some(id) = attribute_id(attribute) {
        match patched(stack, ids::ATTRIBUTE_MODIFIERS) {
            Patched::Value(v) => {
                for m in read_attribute_modifiers(v).unwrap_or_default() {
                    if m.attribute == id && crate::enchant::slot_group_matches(m.slot_group, slot) {
                        out.push((m.operation, m.amount));
                    }
                }
            }
            Patched::Removed => {}
            Patched::Default => {
                for m in item_id(stack).info().modifiers {
                    if m.attribute == id && crate::enchant::slot_group_matches(m.slot, slot) {
                        out.push((m.operation, m.amount));
                    }
                }
            }
        }
    }
    if let Some(data) = crate::player::play::game_data() {
        data.enchants
            .attribute_modifiers(stack, slot, attribute, out);
    }
}

/// `AttributeInstance.calculateValue`: the base plus the modifiers in operation order,
/// clamped to the attribute's range.
pub fn attribute_value(base: f64, modifiers: &[(u8, f64)], min: f64, max: f64) -> f64 {
    let mut v = base;
    for &(op, amount) in modifiers {
        if op == 0 {
            v += amount;
        }
    }
    let mut result = v;
    for &(op, amount) in modifiers {
        if op == 1 {
            result += v * amount;
        }
    }
    for &(op, amount) in modifiers {
        if op == 2 {
            result *= 1.0 + amount;
        }
    }
    result.clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sword_attack_damage_and_speed() {
        let sword = ItemStack::new(vanillite_data::item::ids::DIAMOND_SWORD.0, 1);
        let mut damage = Vec::new();
        attribute_modifiers(
            &sword,
            EquipmentSlot::MainHand,
            "minecraft:attack_damage",
            &mut damage,
        );
        assert!((attribute_value(1.0, &damage, 0.0, 2048.0) - 7.0).abs() < 1e-9);
        let mut speed = Vec::new();
        attribute_modifiers(
            &sword,
            EquipmentSlot::MainHand,
            "minecraft:attack_speed",
            &mut speed,
        );
        assert!((attribute_value(4.0, &speed, 0.0, 1024.0) - 1.6).abs() < 1e-6);
        // In the off hand it does nothing.
        let mut off = Vec::new();
        attribute_modifiers(
            &sword,
            EquipmentSlot::OffHand,
            "minecraft:attack_damage",
            &mut off,
        );
        assert!(off.is_empty());
    }

    #[test]
    fn equippable_defaults_and_patches() {
        use vanillite_data::item::ids as items;
        let helmet = ItemStack::new(items::DIAMOND_HELMET.0, 1);
        let e = equippable(&helmet).expect("helmet equippable");
        assert_eq!(e.slot, EquipmentSlot::Head);
        assert!(e.swappable && e.damage_on_hurt && !e.can_be_sheared);
        assert_eq!(e.equip_sound, "minecraft:item.armor.equip_diamond");
        // Carpets (direct allowed entities: no tags needed without data packs).
        let carpet = ItemStack::new(items::WHITE_CARPET.0, 1);
        let c = equippable(&carpet).expect("carpet equippable");
        assert_eq!(c.slot, EquipmentSlot::Body);
        assert!(c.can_be_sheared && !c.equip_on_interact);
        assert_eq!(c.shearing_sound, "minecraft:item.llama_carpet.unequip");
        let llama = vanillite_data::entity::ids::LLAMA.0;
        assert!(c.can_be_equipped_by(llama));
        assert!(!c.can_be_equipped_by(vanillite_data::entity::ids::HORSE.0));
        // A patched equippable puts a stick on the head; a removed one keeps a helmet in hand.
        let mut stick = ItemStack::new(items::STICK.0, 1);
        let mut wire = vec![4u8]; // slot: head
        wire.push(0); // inline sound event
        let name = "minecraft:item.armor.equip_generic";
        wire.push(u8::try_from(name.len()).unwrap());
        wire.extend_from_slice(name.as_bytes());
        wire.extend([0, 0, 0, 0]); // no range, asset, overlay, allowed entities
        wire.extend([1, 1, 1, 0, 0]); // dispensable, swappable, damage on hurt, interact, shear
        let sound = vanillite_data::registry::SOUND_EVENT.id(name).unwrap();
        vanillite_protocol::codec::write_varint(&mut wire, i32::from(sound) + 1);
        stick.set_component(ids::EQUIPPABLE, wire.into_boxed_slice());
        assert_eq!(equipment_slot(&stick), EquipmentSlot::Head);
        let mut bare = helmet.clone();
        bare.remove_component(ids::EQUIPPABLE);
        assert_eq!(equipment_slot(&bare), EquipmentSlot::MainHand);
        assert!(equippable(&bare).is_none());
    }

    #[test]
    fn patched_modifiers_weapon_and_damage() {
        use vanillite_data::item::ids as items;
        let mut stick = ItemStack::new(items::STICK.0, 1);
        let attack = attribute_id("minecraft:attack_damage").unwrap();
        // One modifier: attribute, id, amount, add_value, mainhand, default display.
        let mut wire = vec![1u8, u8::try_from(attack).unwrap()];
        wire.push(14);
        wire.extend_from_slice(b"minecraft:test");
        wire.extend(3.0f64.to_be_bytes());
        wire.extend([0, 1, 0]);
        stick.set_component(ids::ATTRIBUTE_MODIFIERS, wire.into_boxed_slice());
        let mut damage = Vec::new();
        attribute_modifiers(
            &stick,
            EquipmentSlot::MainHand,
            "minecraft:attack_damage",
            &mut damage,
        );
        assert_eq!(damage, vec![(0, 3.0)]);
        // A sword whose modifiers are removed has none.
        let mut sword = ItemStack::new(items::DIAMOND_SWORD.0, 1);
        sword.remove_component(ids::ATTRIBUTE_MODIFIERS);
        let mut none = Vec::new();
        attribute_modifiers(
            &sword,
            EquipmentSlot::MainHand,
            "minecraft:attack_damage",
            &mut none,
        );
        assert!(none.is_empty());
        // `weapon` from the patch.
        assert!(weapon(&stick).is_none());
        stick.set_component(ids::WEAPON, vec![2, 0, 0, 0, 0].into_boxed_slice());
        assert_eq!(weapon(&stick).map(|w| w.item_damage_per_attack), Some(2));
        // `!damage` makes a sword unbreakable in effect.
        let mut sword = ItemStack::new(items::DIAMOND_SWORD.0, 1);
        assert!(is_damageable(&sword));
        sword.remove_component(ids::DAMAGE);
        assert!(!is_damageable(&sword));
    }

    #[test]
    fn attribute_operations_in_order() {
        // (base + add) * (1 + mult_base) * (1 + mult_total).
        let mods = [(2, 0.5), (0, 2.0), (1, 1.0)];
        assert!((attribute_value(1.0, &mods, 0.0, 100.0) - 9.0).abs() < 1e-9);
    }
}
