//! Jukeboxes (`JukeboxBlock`, `JukeboxBlockEntity`, `JukeboxSongPlayer`): a music disc put in
//! plays its song (level event 1010) until it ends or the disc comes out. While it plays, the
//! jukebox emits `jukebox_play` every second (allays dance) and gives a redstone signal of 15;
//! comparators read the song's comparator output.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockState, ids};
use vanillite_nbt::{Compound, Tag};
use vanillite_worldgen::behaviour::StateExt;

use super::{BlockBehaviour, Interaction, ItemInteraction, UseContext};
use crate::item::ItemStack;
use crate::level::game_event::{GameEventContext, events};
use crate::level::{BlockEntity, Level, update};

/// A `minecraft:jukebox_song` registry entry (`JukeboxSong`).
#[derive(Clone, Debug, PartialEq)]
pub struct JukeboxSong {
    /// The registry name.
    pub name: String,
    /// `lengthInSeconds`.
    pub length_in_seconds: f32,
    /// `comparatorOutput`.
    pub comparator_output: i32,
}

impl JukeboxSong {
    /// Reads a registry entry's JSON.
    pub fn from_json(name: &str, json: &serde_json::Value) -> JukeboxSong {
        JukeboxSong {
            name: name.to_owned(),
            length_in_seconds: json["length_in_seconds"].as_f64().unwrap_or(0.0) as f32,
            comparator_output: json["comparator_output"].as_i64().unwrap_or(0) as i32,
        }
    }

    /// `lengthInTicks`.
    pub fn length_in_ticks(&self) -> i64 {
        (self.length_in_seconds * 20.0).ceil() as i64
    }

    /// `hasFinished(ticksElapsed)`.
    pub fn has_finished(&self, ticks: i64) -> bool {
        ticks >= self.length_in_ticks() + 20
    }
}

/// The data of a `JukeboxBlockEntity` with its `JukeboxSongPlayer`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Jukebox {
    /// The disc.
    pub item: ItemStack,
    /// The playing song (`song`): a registry entry or the disc's inline song.
    pub song: Option<JukeboxSong>,
    /// `ticksSinceSongStarted`.
    pub ticks_since_song_started: i64,
}

impl Jukebox {
    /// `loadAdditional` (on a fresh block entity).
    pub fn load(&mut self, nbt: &Compound) {
        self.item = match nbt.get("RecordItem") {
            Some(Tag::Compound(c)) => crate::inventory::stack_from_nbt(c),
            _ => ItemStack::EMPTY,
        };
        if let Some(Tag::Long(ticks)) = nbt.get("ticks_since_song_started")
            && let Some(s) = song_from_stack(&self.item)
            && !s.has_finished(*ticks)
        {
            // `setSongWithoutPlaying`.
            self.song = Some(s);
            self.ticks_since_song_started = *ticks;
        }
    }

    /// `saveAdditional`.
    pub fn save(&self, out: &mut Compound) {
        if !self.item.is_empty() {
            out.insert("RecordItem", crate::inventory::stack_to_nbt(&self.item));
        }
        if self.song.is_some() {
            out.insert(
                "ticks_since_song_started",
                Tag::Long(self.ticks_since_song_started),
            );
        }
    }

    /// `getComparatorOutput`: the comparator output of the disc's song.
    pub fn comparator_output(&self) -> i32 {
        song_from_stack(&self.item).map_or(0, |s| s.comparator_output)
    }
}

/// The song of a registry id.
fn song<R>(id: u32, f: impl FnOnce(&JukeboxSong) -> R) -> Option<R> {
    crate::player::play::game_data()?
        .jukebox_songs
        .get(usize::try_from(id).ok()?)
        .map(f)
}

/// `JukeboxSong.fromStack` with the song itself: a registry entry, or the inline
/// (`Holder.direct`) song of a patched `jukebox_playable`.
pub fn song_from_stack(stack: &ItemStack) -> Option<JukeboxSong> {
    use vanillite_protocol::components::ids::JUKEBOX_PLAYABLE;
    if let Some(v) = stack.patch().and_then(|p| p.get(JUKEBOX_PLAYABLE))
        && !stack.is_empty()
    {
        let mut r = vanillite_protocol::codec::Reader::new(v);
        if r.varint().ok()? == 0 {
            return inline_song(&mut r);
        }
    }
    song_of(stack).and_then(|id| song(id, Clone::clone))
}

/// `JukeboxSong.DIRECT_STREAM_CODEC`: the sound event (holder), the description (a text
/// component), `length_in_seconds` and `comparator_output`. The name stays empty.
fn inline_song(r: &mut vanillite_protocol::codec::Reader<'_>) -> Option<JukeboxSong> {
    // `SoundEvent.STREAM_CODEC`: a registry id + 1, or 0 with the location and the range.
    if r.varint().ok()? == 0 {
        r.string(32767).ok()?;
        if r.u8().ok()? != 0 {
            r.f32().ok()?;
        }
    }
    let mut rest = r.take_rest();
    vanillite_nbt::read::read_network(&mut rest, &mut vanillite_nbt::read::Budget::network())
        .ok()?;
    let mut r = vanillite_protocol::codec::Reader::new(rest);
    Some(JukeboxSong {
        name: String::new(),
        length_in_seconds: r.f32().ok()?,
        comparator_output: r.varint().ok()?,
    })
}

/// The registry id of a song (`-1` for an inline song, `Registry.getId` of a direct holder).
fn song_id(s: &JukeboxSong) -> i32 {
    crate::player::play::game_data()
        .and_then(|d| {
            d.jukebox_songs
                .iter()
                .position(|e| e.name == s.name && !s.name.is_empty())
        })
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1)
}

/// `JukeboxSong.fromStack`: the registry id of the stack's `jukebox_playable` song (a patched
/// holder or the default of the music discs). Inline songs are not registered and give none.
pub fn song_of(stack: &ItemStack) -> Option<u32> {
    use vanillite_protocol::components::ids::JUKEBOX_PLAYABLE;
    if stack.is_empty() {
        return None;
    }
    if let Some(p) = stack.patch() {
        if let Some(v) = p.get(JUKEBOX_PLAYABLE) {
            let holder = vanillite_protocol::codec::Reader::new(v).varint().ok()?;
            return u32::try_from(holder.checked_sub(1)?).ok();
        }
        if p.is_removed(JUKEBOX_PLAYABLE) {
            return None;
        }
    }
    // The music discs: `music_disc_<song>` plays `minecraft:<song>`.
    let name = crate::item::item_id(stack).name();
    let song_name = format!("minecraft:{}", name.strip_prefix("minecraft:music_disc_")?);
    crate::player::play::game_data()?
        .jukebox_songs
        .iter()
        .position(|s| s.name == song_name)
        .and_then(|i| u32::try_from(i).ok())
}

/// Whether a stack has the `jukebox_playable` component.
pub fn is_playable(stack: &ItemStack) -> bool {
    use vanillite_protocol::components::ids::JUKEBOX_PLAYABLE;
    match stack.patch() {
        Some(p) if p.get(JUKEBOX_PLAYABLE).is_some() => true,
        Some(p) if p.is_removed(JUKEBOX_PLAYABLE) => false,
        _ => {
            !stack.is_empty()
                && crate::item::item_id(stack)
                    .name()
                    .starts_with("minecraft:music_disc_")
        }
    }
}

/// Fills the jukebox behaviours.
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    if block == ids::JUKEBOX {
        b.signal = Some(signal);
        b.analog_output = Some(analog_output);
        b.use_without_item = Some(use_without_item);
        b.use_item_on = Some(use_item_on);
        b.affect_neighbors_after_removal = Some(affect_neighbors_after_removal);
    }
}

fn jukebox(level: &Level, pos: BlockPos) -> Option<&Jukebox> {
    match level.block_entity(pos) {
        Some(BlockEntity::Jukebox(j)) => Some(j),
        _ => None,
    }
}

fn jukebox_mut(level: &mut Level, pos: BlockPos) -> Option<&mut Jukebox> {
    match level.block_entity_mut(pos) {
        Some(BlockEntity::Jukebox(j)) => Some(j),
        _ => None,
    }
}

/// `ownSignal`: 15 while a song plays.
fn signal(level: &Level, _: BlockState, pos: BlockPos, _: Direction) -> i32 {
    if jukebox(level, pos).is_some_and(|j| j.song.is_some()) {
        15
    } else {
        0
    }
}

/// `getAnalogOutputSignal`.
fn analog_output(level: &Level, _: BlockState, pos: BlockPos, _: Direction) -> i32 {
    jukebox(level, pos).map_or(0, Jukebox::comparator_output)
}

/// `useWithoutItem`: a disc pops out.
fn use_without_item(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    _ctx: &UseContext,
) -> Interaction {
    if state.flag("has_record") && jukebox(level, pos).is_some() {
        pop_out_the_item(level, pos);
        Interaction::Success
    } else {
        Interaction::Pass
    }
}

/// `useItemOn` with `JukeboxPlayable.tryInsertIntoJukebox`: a disc goes in and plays.
fn use_item_on(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    if state.flag("has_record") || !is_playable(stack) {
        return ItemInteraction::TryWithEmptyHand;
    }
    // `consumeAndReturn(1, player)`: creative players keep theirs.
    let inserted = if ctx.infinite {
        stack.with_count(1)
    } else {
        stack.split(1)
    };
    if jukebox(level, pos).is_some() {
        set_the_item(level, pos, inserted);
        level.game_event_block(
            events().block_change,
            pos,
            GameEventContext::with_state(Some(ctx.player), state),
        );
    }
    level.award_stat(ctx.player, crate::stats::custom::PLAY_RECORD, 1);
    ItemInteraction::Done(Interaction::Success)
}

/// `affectNeighborsAfterRemoval`: `Containers.updateNeighboursAfterDestroy`.
fn affect_neighbors_after_removal(level: &mut Level, state: BlockState, pos: BlockPos, _: bool) {
    level.update_neighbour_for_output_signal(pos, state.block());
}

/// `onSongChanged`: neighbours and comparators update.
fn on_song_changed(level: &mut Level, pos: BlockPos) {
    let state = level.block(pos);
    level.update_neighbors_at(pos, state.block());
    super::container::set_changed(level, pos);
}

/// `setTheItem`: the jukebox shows the disc and plays its song, or stops.
pub fn set_the_item(level: &mut Level, pos: BlockPos, item: ItemStack) {
    let song = song_from_stack(&item);
    let inserted = !item.is_empty();
    let Some(j) = jukebox_mut(level, pos) else {
        return;
    };
    j.item = item;
    level.block_entities.touch(pos.chunk());
    // `notifyItemChangedInJukebox`: only while the block is still the jukebox.
    let state = level.block(pos);
    if state.is(ids::JUKEBOX) {
        let new = state.set_flag("has_record", inserted);
        level.set_block(pos, new, update::CLIENTS);
        level.game_event_block(
            events().block_change,
            pos,
            GameEventContext {
                source: None,
                affected: Some(new),
                owner: None,
            },
        );
    }
    match song {
        Some(s) if inserted => play(level, pos, s),
        _ => stop(level, pos),
    }
}

/// `JukeboxSongPlayer.play`.
fn play(level: &mut Level, pos: BlockPos, s: JukeboxSong) {
    let id = song_id(&s);
    if let Some(j) = jukebox_mut(level, pos) {
        j.song = Some(s);
        j.ticks_since_song_started = 0;
    }
    level.level_event(None, 1010, pos, id);
    on_song_changed(level, pos);
}

/// `JukeboxSongPlayer.stop`.
fn stop(level: &mut Level, pos: BlockPos) {
    let Some(j) = jukebox_mut(level, pos) else {
        return;
    };
    if j.song.take().is_none() {
        return;
    }
    j.ticks_since_song_started = 0;
    let state = level.block(pos);
    level.game_event_block(
        events().jukebox_stop_play,
        pos,
        GameEventContext {
            source: None,
            affected: Some(state),
            owner: None,
        },
    );
    level.level_event(None, 1011, pos, 0);
    on_song_changed(level, pos);
}

/// `popOutTheItem`: the disc jumps out on top.
pub fn pop_out_the_item(level: &mut Level, pos: BlockPos) {
    let Some(item) = jukebox(level, pos).map(|j| j.item.clone()) else {
        return;
    };
    if item.is_empty() {
        return;
    }
    // `removeTheItem` → `setTheItem(EMPTY)`.
    set_the_item(level, pos, ItemStack::EMPTY);
    // `atLowerCornerWithOffset(pos, 0.5, 1.01, 0.5).offsetRandomXZ(random, 0.7)`.
    let random = level.random();
    let dx = (random.next_float() - 0.5) * 0.7;
    let dz = (random.next_float() - 0.5) * 0.7;
    let at = Vec3::new(
        f64::from(pos.x) + 0.5 + f64::from(dx),
        f64::from(pos.y) + 1.01,
        f64::from(pos.z) + 0.5 + f64::from(dz),
    );
    super::drops::spawn_item_moving(level, at, item, None, 10, None);
    on_song_changed(level, pos);
}

/// `JukeboxBlockEntity.tick` (`getTicker` only while it has a record).
pub fn tick(level: &mut Level, pos: BlockPos, state: BlockState) {
    if !state.flag("has_record") {
        return;
    }
    let Some(j) = jukebox(level, pos) else {
        return;
    };
    let Some(s) = &j.song else {
        return;
    };
    let ticks = j.ticks_since_song_started;
    if s.has_finished(ticks) {
        stop(level, pos);
        return;
    }
    if ticks % 20 == 0 {
        level.game_event_block(
            events().jukebox_play,
            pos,
            GameEventContext {
                source: None,
                affected: Some(state),
                owner: None,
            },
        );
        // `spawnMusicParticles`: a note above the jukebox in one of four colours.
        #[allow(clippy::cast_precision_loss)]
        let color = level.random().next_int_bounded(4) as f32 / 24.0;
        let at = vanillite_base::Vec3::new(
            f64::from(pos.x) + 0.5,
            f64::from(pos.y) + f64::from(1.2f32),
            f64::from(pos.z) + 0.5,
        );
        level.send_particles(
            crate::level::particles::simple("minecraft:note"),
            at,
            0,
            vanillite_base::Vec3::new(f64::from(color), 0.0, 0.0),
            1.0,
        );
    }
    if let Some(j) = jukebox_mut(level, pos) {
        j.ticks_since_song_started += 1;
    }
}

/// `setRemoved` of a removed jukebox: listeners and clients hear it stop.
pub fn removed(level: &mut Level, pos: BlockPos, state: BlockState) {
    level.game_event_block(
        events().jukebox_stop_play,
        pos,
        GameEventContext {
            source: None,
            affected: Some(state),
            owner: None,
        },
    );
    level.level_event(None, 1011, pos, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An inline (`Holder.direct`) song of a patched `jukebox_playable` (#272 I5-17): the
    /// jukebox keeps its length and comparator output; its level event id is -1.
    #[test]
    fn inline_song_plays() {
        use vanillite_protocol::components::ids::JUKEBOX_PLAYABLE;
        let mut v = vec![0, 1, 8, 0, 1, b'x'];
        v.extend_from_slice(&10.0f32.to_be_bytes());
        v.push(7);
        let mut disc = ItemStack::new(
            vanillite_data::registry::ITEM
                .id("minecraft:stick")
                .unwrap_or(1),
            1,
        );
        disc.set_component(JUKEBOX_PLAYABLE, v.into());
        let s = song_from_stack(&disc).expect("inline song");
        assert_eq!((s.length_in_seconds, s.comparator_output), (10.0, 7));
        assert_eq!(song_id(&s), -1);
        assert!(is_playable(&disc));
        let j = Jukebox {
            item: disc,
            ..Jukebox::default()
        };
        assert_eq!(j.comparator_output(), 7);
    }
}
