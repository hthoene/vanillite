//! Signs and hanging signs (`SignBlock`, `SignBlockEntity`): two sides of four lines each,
//! edited by one player at a time, coloured by dyes, glowing with glow ink, locked by wax.
//!
//! Click commands in sign text are not run (signs written by players carry plain text only).

use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockKind, BlockState};
use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::text::Component;
use vanillite_worldgen::behaviour::StateExt;

use super::{BlockBehaviour, Interaction, ItemInteraction, UseContext, sound_id};
use crate::item::ItemStack;
use crate::level::{BlockEntity, Level};

/// `SoundSource.BLOCKS`.
const BLOCKS: u8 = 4;
/// The 16 dye colours in `DyeColor` order.
pub const DYE_COLORS: [&str; 16] = [
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];

/// One side of a sign (`SignText`).
#[derive(Clone, Debug, PartialEq)]
pub struct SignText {
    /// The four lines.
    pub messages: [Component; 4],
    /// `color`: a dye colour name.
    pub color: &'static str,
    /// `has_glowing_text`.
    pub glowing: bool,
}

impl Default for SignText {
    fn default() -> Self {
        SignText {
            messages: std::array::from_fn(|_| Component::empty()),
            color: "black",
            glowing: false,
        }
    }
}

impl SignText {
    /// `hasEditableText`: every line is empty or plain text.
    pub fn has_editable_text(&self) -> bool {
        self.messages
            .iter()
            .all(|m| matches!(m.content, vanillite_protocol::text::Content::Text(_)))
    }

    /// The `sign_text_front`/`sign_text_back` component value of `stack` (`SignText.EMPTY`
    /// without it).
    pub fn of_component(stack: &ItemStack, id: u16) -> SignText {
        stack
            .component(id)
            .and_then(|w| crate::item_nbt::component_to_nbt(id, w))
            .and_then(|t| match t {
                Tag::Compound(c) => Some(SignText::load(&c)),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// This side as a `sign_text_front`/`sign_text_back` component value.
    pub fn component(&self, id: u16) -> Option<Box<[u8]>> {
        crate::item_nbt::component_from_nbt(id, &Tag::Compound(self.save()))
    }

    /// `hasMessage`: any non-empty line.
    fn has_message(&self) -> bool {
        self.messages.iter().any(|m| !m.plain().is_empty())
    }

    pub(crate) fn save(&self) -> Compound {
        let mut c = Compound::new();
        let mut list = List::new();
        for m in &self.messages {
            list.0.push(m.to_nbt());
        }
        c.insert("messages", list);
        c.insert("color", Tag::String(self.color.to_owned()));
        c.insert("has_glowing_text", Tag::Byte(i8::from(self.glowing)));
        c.sort_java_hash_order();
        c
    }

    pub(crate) fn load(c: &Compound) -> SignText {
        let mut text = SignText::default();
        if let Some(Tag::List(list)) = c.get("messages") {
            for (slot, tag) in text.messages.iter_mut().zip(&list.0) {
                *slot = Component::from_nbt(tag);
            }
        }
        if let Some(Tag::String(color)) = c.get("color") {
            text.color = DYE_COLORS
                .iter()
                .copied()
                .find(|d| d == color)
                .unwrap_or("black");
        }
        text.glowing = matches!(c.get("has_glowing_text"), Some(Tag::Byte(v)) if *v != 0);
        text
    }
}

/// A sign block entity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sign {
    /// `front_text`.
    pub front: SignText,
    /// `back_text`.
    pub back: SignText,
    /// `is_waxed`.
    pub waxed: bool,
    /// `playerWhoMayEdit`.
    pub editor: Option<u128>,
    /// `allow_op_features`: the click commands run (signs of old worlds, set by the data fixers).
    pub allow_op_features: bool,
}

impl Sign {
    /// One side.
    pub fn text_mut(&mut self, front: bool) -> &mut SignText {
        if front {
            &mut self.front
        } else {
            &mut self.back
        }
    }

    /// The stored and client data (`saveAdditional`, `getUpdateTag`).
    pub fn save(&self, out: &mut Compound) {
        if self.allow_op_features {
            out.insert("allow_op_features", Tag::Byte(1));
        }
        out.insert("front_text", self.front.save());
        out.insert("back_text", self.back.save());
        out.insert("is_waxed", Tag::Byte(i8::from(self.waxed)));
    }

    /// Loads the stored fields.
    pub fn load(&mut self, nbt: &Compound) {
        self.allow_op_features =
            matches!(nbt.get("allow_op_features"), Some(Tag::Byte(v)) if *v != 0);
        if let Some(Tag::Compound(c)) = nbt.get("front_text") {
            self.front = SignText::load(c);
        }
        if let Some(Tag::Compound(c)) = nbt.get("back_text") {
            self.back = SignText::load(c);
        }
        self.waxed = matches!(nbt.get("is_waxed"), Some(Tag::Byte(v)) if *v != 0);
    }
}

/// Whether a block is a sign or hanging sign.
pub fn is_sign(block: BlockId) -> bool {
    matches!(
        block.info().kind,
        BlockKind::StandingSignBlock
            | BlockKind::WallSignBlock
            | BlockKind::CeilingHangingSignBlock
            | BlockKind::WallHangingSignBlock
    )
}

/// Fills the sign behaviours.
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    if is_sign(block) {
        b.use_without_item = Some(use_without_item);
        b.use_item_on = Some(use_item_on);
    }
}

/// `getYRotationDegrees`.
fn y_rotation(state: BlockState) -> f32 {
    if let Some(rotation) = state.int("rotation") {
        #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
        return rotation as f32 * 22.5;
    }
    match state.direction("facing") {
        Some(Direction::West) => 90.0,
        Some(Direction::North) => 180.0,
        Some(Direction::East) => 270.0,
        _ => 0.0,
    }
}

/// `SignBlockEntity.isFacingFrontText` / `getSlotPlayerIsFacing`.
pub fn facing_front(state: BlockState, pos: BlockPos, player: Vec3) -> bool {
    let dx = player.x - (f64::from(pos.x) + 0.5);
    let dz = player.z - (f64::from(pos.z) + 0.5);
    let sign = y_rotation(state);
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    let angle = (dz.atan2(dx) * 180.0 / std::f64::consts::PI) as f32 - 90.0;
    // `Mth.degreesDifferenceAbs`.
    let mut diff = (angle - sign) % 360.0;
    if diff >= 180.0 {
        diff -= 360.0;
    }
    if diff < -180.0 {
        diff += 360.0;
    }
    diff.abs() <= 90.0
}

fn player_pos(level: &Level, context: &UseContext) -> Vec3 {
    level
        .entities
        .store
        .get(context.player)
        .map_or(context.hit, |e| e.pos())
}

fn player_uuid(level: &Level, context: &UseContext) -> Option<u128> {
    level.entities.store.get(context.player).map(|e| e.uuid())
}

/// `SignBlock.otherPlayerIsEditingSign`.
fn other_editing(sign: &Sign, uuid: Option<u128>) -> bool {
    sign.editor.is_some_and(|e| Some(e) != uuid)
}

/// `SignBlock.useWithoutItem`: waxed signs refuse, otherwise the editor opens.
fn use_without_item(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    context: &UseContext,
) -> Interaction {
    let front = facing_front(state, pos, player_pos(level, context));
    let uuid = player_uuid(level, context);
    let Some(BlockEntity::Sign(sign)) = level.block_entity_mut(pos) else {
        return Interaction::Pass;
    };
    if sign.waxed {
        let sound = if matches!(
            state.block().info().kind,
            BlockKind::CeilingHangingSignBlock | BlockKind::WallHangingSignBlock
        ) {
            "block.hanging_sign.waxed_interact_fail"
        } else {
            "block.sign.waxed_interact_fail"
        };
        level.play_block_sound(None, pos, sound_id(sound), BLOCKS, 1.0, 1.0);
        return Interaction::Success;
    }
    let editable = sign
        .text_mut(front)
        .messages
        .iter()
        .all(|m| matches!(m.content, vanillite_protocol::text::Content::Text(_)));
    if !other_editing(sign, uuid) && context.may_build && editable {
        sign.editor = uuid;
        level.request_sign_editor(context.player, pos, front);
        return Interaction::Success;
    }
    Interaction::Pass
}

/// The dye colour of a stack's `dye` component (patched or the item's default).
pub(crate) fn dye_color(stack: &ItemStack) -> Option<&'static str> {
    let id = crate::menu::station::dye_color(stack)?;
    DYE_COLORS.get(usize::try_from(id).ok()?).copied()
}

/// `SignBlock.useItemOn`: dyes, glow ink, ink and honeycomb (`SignApplicator`).
fn use_item_on(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    context: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    use vanillite_data::item::ids as items;
    let id = crate::item::item_id(stack);
    // `shouldTryToChainAnotherHangingSign`: a hanging sign item used on the underside of a
    // ceiling hanging sign or on a plank end of a wall hanging sign places the next sign.
    let hanging_item = id
        .block()
        .is_some_and(|b| b.info().kind == BlockKind::CeilingHangingSignBlock);
    let axis = |d: Direction| match d {
        Direction::East | Direction::West => 0,
        Direction::Down | Direction::Up => 1,
        Direction::North | Direction::South => 2,
    };
    let chains = match state.block().info().kind {
        BlockKind::CeilingHangingSignBlock => context.face == Direction::Down,
        BlockKind::WallHangingSignBlock => {
            let facing = state.direction("facing").unwrap_or(Direction::North);
            axis(context.face) != axis(facing)
        }
        _ => false,
    };
    if hanging_item && chains {
        return ItemInteraction::Pass;
    }
    let dye = dye_color(stack);
    let applicator = dye.is_some()
        || id == items::GLOW_INK_SAC
        || id == items::INK_SAC
        || id == items::HONEYCOMB;
    let front = facing_front(state, pos, player_pos(level, context));
    let uuid = player_uuid(level, context);
    let Some(BlockEntity::Sign(sign)) = level.block_entity_mut(pos) else {
        return ItemInteraction::Pass;
    };
    if !(applicator && context.may_build) || sign.waxed || other_editing(sign, uuid) {
        return ItemInteraction::TryWithEmptyHand;
    }
    let (sound, changed) = if id == items::HONEYCOMB {
        let changed = !sign.waxed;
        sign.waxed = true;
        ("item.honeycomb.wax_on", changed)
    } else {
        let text = sign.text_mut(front);
        // `SignApplicator.canApplyToSign`: only on text.
        if !text.has_message() {
            return ItemInteraction::TryWithEmptyHand;
        }
        match dye {
            Some(color) => {
                let changed = text.color != color;
                text.color = color;
                ("item.dye.use", changed)
            }
            None => {
                let glow = id == items::GLOW_INK_SAC;
                let changed = text.glowing != glow;
                text.glowing = glow;
                (
                    if glow {
                        "item.glow_ink_sac.use"
                    } else {
                        "item.ink_sac.use"
                    },
                    changed,
                )
            }
        }
    };
    if !changed {
        return ItemInteraction::TryWithEmptyHand;
    }
    if id == items::HONEYCOMB {
        level.level_event(None, 3003, pos, 0);
    }
    level.play_block_sound(None, pos, sound_id(sound), BLOCKS, 1.0, 1.0);
    level.block_entity_changed(pos);
    let events = crate::level::game_event::events();
    let player = context.player;
    let context = crate::level::game_event::GameEventContext::with_state(Some(player), state);
    level.game_event_block(events.block_change, pos, context);
    let used = crate::stats::Stat::of(crate::stats::StatType::Used, id.0);
    level.award_stat(player, used, 1);
    stack.shrink(1);
    ItemInteraction::Done(Interaction::Success)
}

/// `updateSignText` of `ServerboundSignUpdatePacket`: the editing player sets one side.
pub fn update_text(
    level: &mut Level,
    pos: BlockPos,
    player: u128,
    front: bool,
    lines: &[String; 4],
) {
    let Some(BlockEntity::Sign(sign)) = level.block_entity(pos) else {
        return;
    };
    if sign.waxed || sign.editor != Some(player) {
        log::warn!("A player just tried to change a non-editable sign");
        return;
    }
    // `ChatFormatting.stripFormatting`.
    let mut plain: [String; 4] = std::array::from_fn(|i| strip_formatting(&lines[i]));
    if let Some(hooks) = level.hooks_for(vanillite_abi::event::SIGN_CHANGE) {
        let mut ev = crate::hooks::Event {
            player,
            pos,
            state: Some(level.block(pos)),
            flags: u32::from(front),
            text: serde_json::to_string(&plain).unwrap_or_default(),
            ..crate::hooks::Event::new(vanillite_abi::event::SIGN_CHANGE)
        };
        hooks.event(crate::hooks::HookWorld::of(level), &mut ev);
        if ev.cancelled {
            // Bukkit's cancelled `SignChangeEvent`: the text stays; the client is corrected.
            if let Some(BlockEntity::Sign(sign)) = level.block_entity_mut(pos) {
                sign.editor = None;
            }
            level.block_entity_changed(pos);
            return;
        }
        if let Some(new) = ev.text_out.as_ref().and_then(|t| plugin_lines(t.as_str())) {
            plain = new;
        }
    }
    let Some(BlockEntity::Sign(sign)) = level.block_entity_mut(pos) else {
        return;
    };
    let text = sign.text_mut(front);
    for (slot, plain) in text.messages.iter_mut().zip(plain) {
        // Keeping the line's style.
        *slot = Component {
            content: vanillite_protocol::text::Content::Text(plain),
            style: slot.style.clone(),
            extra: Vec::new(),
        };
    }
    sign.editor = None;
    level.block_entity_changed(pos);
}

/// The most characters of a sign line a plugin may set (the line limit of
/// `ServerboundSignUpdatePacket`).
const MAX_PLUGIN_LINE: usize = 384;

/// Sign lines from a plugin: a JSON array of up to four strings (missing lines empty, longer
/// lines cut, formatting codes removed); `None` for anything else.
fn plugin_lines(json: &str) -> Option<[String; 4]> {
    // Four escaped lines at most (`\uXXXX` is six bytes a character).
    if json.len() > 4 * (MAX_PLUGIN_LINE * 6 + 4) + 2 {
        return None;
    }
    let lines: Vec<String> = serde_json::from_str(json).ok()?;
    if lines.len() > 4 {
        return None;
    }
    Some(std::array::from_fn(|i| {
        lines.get(i).map_or_else(String::new, |line| {
            strip_formatting(&line.chars().take(MAX_PLUGIN_LINE).collect::<String>())
        })
    }))
}

/// `ChatFormatting.stripFormatting`: removes `§` and the code after it.
fn strip_formatting(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{a7}' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// `SignBlockEntity.tick`: an editor that left or went away loses the lock.
pub fn tick(level: &mut Level, pos: BlockPos, _state: BlockState) {
    let Some(BlockEntity::Sign(sign)) = level.block_entity(pos) else {
        return;
    };
    let Some(editor) = sign.editor else {
        return;
    };
    let center = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    let near = level.entities.players.iter().any(|(id, _)| {
        level.entities.store.get(id).is_some_and(|e| {
            let p = e.pos();
            // `isWithinBlockInteractionRange(pos, 4)` with the creative reach as bound.
            e.uuid() == editor
                && (p.x - center.x).powi(2) + (p.y - center.y).powi(2) + (p.z - center.z).powi(2)
                    < 10.5 * 10.5
        })
    });
    if !near && let Some(BlockEntity::Sign(sign)) = level.block_entity_mut(pos) {
        sign.editor = None;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use vanillite_protocol::components::ids;

    /// `sign_text_front`/`sign_text_back` round trip (`SignText.DIRECT_CODEC`) and
    /// `SignText.hasEditableText` (#272, I6-32).
    #[test]
    fn sign_text_components() {
        let mut text = SignText {
            color: "red",
            glowing: true,
            ..SignText::default()
        };
        text.messages[1] = Component::text("hi");
        let mut stack = ItemStack::new(
            vanillite_data::registry::ITEM
                .id("minecraft:oak_sign")
                .unwrap(),
            1,
        );
        stack.set_component(
            ids::SIGN_TEXT_FRONT,
            text.component(ids::SIGN_TEXT_FRONT).unwrap(),
        );
        assert_eq!(SignText::of_component(&stack, ids::SIGN_TEXT_FRONT), text);
        assert_eq!(
            SignText::of_component(&stack, ids::SIGN_TEXT_BACK),
            SignText::default()
        );
        assert!(text.has_editable_text());
        text.messages[2] = Component::translatable("block.minecraft.stone", Vec::new());
        assert!(!text.has_editable_text());
    }
}
