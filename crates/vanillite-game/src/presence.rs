//! Players of other worlds as a world sees them (`ServerLevel.getPlayerInAnyDimension`): where
//! a projectile's owner is after it went through a portal.
//!
//! Each world publishes its players at the end of its tick; a reader sees what was published
//! in the ticks before its own, so every world reads the same values whatever order (or
//! threads) the worlds tick in, as Vanilla's levels see the players after the previous
//! connection tick.

use std::sync::{Arc, Mutex, PoisonError};

use vanillite_base::Vec3;

/// An online player as another world sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RemotePlayer {
    /// `getEyePosition()`.
    pub eye: Vec3,
    /// `isAlive()`.
    pub alive: bool,
    /// `isSpectator()`.
    pub spectator: bool,
}

/// What a player's world last published of it ([`Presence::publish`]).
#[derive(Debug, Default)]
pub struct Presence(Mutex<Slots>);

#[derive(Debug, Default)]
struct Slots {
    /// The server tick of `now`.
    tick: u64,
    now: Option<RemotePlayer>,
    /// The value of the tick before `tick`.
    before: Option<RemotePlayer>,
}

impl Presence {
    /// The player as it is at the end of server tick `tick`.
    pub fn publish(&self, tick: u64, player: RemotePlayer) {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if s.tick != tick {
            s.before = s.now;
            s.tick = tick;
        }
        s.now = Some(player);
    }

    /// The player as published before server tick `tick`.
    pub fn read(&self, tick: u64) -> Option<RemotePlayer> {
        let s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if s.tick >= tick { s.before } else { s.now }
    }
}

/// The online players of the other worlds, by UUID.
pub trait PlayersElsewhere: Send + Sync {
    /// The player `uuid` when it is online in another world.
    fn find(&self, uuid: u128) -> Option<RemotePlayer>;
    /// The server tick the world runs now (what [`Self::find`] reads is from before it).
    fn set_tick(&self, _tick: u64) {}
}

/// The [`PlayersElsewhere`] of a world's entities; none without other worlds.
#[derive(Clone, Default)]
pub struct Elsewhere(pub Option<Arc<dyn PlayersElsewhere>>);

impl std::fmt::Debug for Elsewhere {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Elsewhere"
        } else {
            "Nowhere"
        })
    }
}

impl Elsewhere {
    /// The player `uuid` online in another world.
    pub fn find(&self, uuid: u128) -> Option<RemotePlayer> {
        self.0.as_ref()?.find(uuid)
    }

    /// [`PlayersElsewhere::set_tick`].
    pub fn set_tick(&self, tick: u64) {
        if let Some(e) = &self.0 {
            e.set_tick(tick);
        }
    }
}

/// Publishes where every player of the world is at the end of server tick `tick`.
pub fn publish(entities: &crate::entities::Entities, tick: u64) {
    for (id, p) in entities.players.iter() {
        let Some(e) = entities.store.get(id) else {
            continue;
        };
        let pos = e.pos();
        let eye = crate::survival::player::eye_height(e.dims().height);
        p.connection.presence.publish(
            tick,
            RemotePlayer {
                eye: Vec3::new(pos.x, pos.y + eye, pos.z),
                alive: !p.survival.is_dead_or_dying(),
                spectator: p.game_mode == vanillite_config::GameMode::Spectator,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader in tick 5 sees what was published in tick 4, whether the owner's world
    /// already ticked 5 or not.
    #[test]
    fn readers_see_the_tick_before() {
        let p = Presence::default();
        let at = |x: f64| RemotePlayer {
            eye: Vec3::new(x, 0.0, 0.0),
            alive: true,
            spectator: false,
        };
        p.publish(4, at(4.0));
        assert_eq!(p.read(5), Some(at(4.0)));
        p.publish(5, at(5.0));
        assert_eq!(p.read(5), Some(at(4.0)));
        assert_eq!(p.read(6), Some(at(5.0)));
    }
}
