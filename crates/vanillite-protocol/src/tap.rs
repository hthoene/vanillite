//! The packet tap: where listeners (plugins) observe, cancel and rewrite packets between the
//! game and the wire, without giving up encode-once fan-out.
//!
//! The network layer asks the tap about every packet it writes or reads. The question is one
//! load and one branch on a bit mask ([`PacketTap::active`], [`PacketTap::wants`]); only a
//! packet whose type has a listener reaches the [`Interceptor`].
//!
//! Clientbound packets pass two stages:
//!
//! 1. **Broadcast**: once per encoded buffer. For a [`SharedPacket`] the outcome is kept with
//!    the packet ([`PacketTap::shared`]), so a broadcast to a thousand receivers runs these
//!    listeners once and still shares one buffer, also when it was replaced.
//! 2. **Receiver**: once per receiver. Only a receiver whose packet a listener replaced gets a
//!    buffer of its own.
//!
//! Listeners see the server's own protocol version; translation for older clients and other
//! editions happens after the tap (clientbound) and before it (serverbound).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::codec::Reader;
use crate::{Direction, SharedPacket, State};

/// Packet ids per state and direction the mask holds.
pub const MAX_IDS: usize = 256;

const WORDS: usize = MAX_IDS / 64;
const STATES: usize = 5;
const TABLES: usize = STATES * 3;

/// When a listener runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Clientbound, once per encoded buffer, before the receivers are known.
    Broadcast,
    /// Clientbound, once per receiver.
    Receiver,
    /// Serverbound.
    Serverbound,
}

/// What the listeners decided about a packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict<B> {
    /// Unchanged.
    Pass,
    /// Cancelled: the packet is not sent, or not handled.
    Drop,
    /// Replaced by this body (`id` + fields).
    Replace(B),
}

/// The connection a packet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Peer {
    /// A number unique to the connection in this process.
    pub connection: u64,
    /// The player's UUID; 0 until the login is finished.
    pub player: u128,
}

/// Runs the listeners. Called from network threads, for many connections at once.
pub trait Interceptor: Send + Sync {
    /// The broadcast stage of a clientbound packet: the listeners that do not depend on the
    /// receiver.
    fn broadcast(&self, state: State, id: i32, body: &[u8]) -> Verdict<Vec<u8>>;
    /// The receiver stage of a clientbound packet.
    fn clientbound(&self, peer: Peer, state: State, id: i32, body: &[u8]) -> Verdict<Vec<u8>>;
    /// A serverbound packet, before it is decoded and handled.
    fn serverbound(&self, peer: Peer, state: State, id: i32, body: &[u8]) -> Verdict<Vec<u8>>;
}

/// The packet types with listeners, as the [`Interceptor`]'s owner computes them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mask {
    bits: [[u64; WORDS]; TABLES],
}

const fn table(stage: Stage, state: State) -> usize {
    let state = match state {
        State::Handshake => 0,
        State::Status => 1,
        State::Login => 2,
        State::Configuration => 3,
        State::Play => 4,
    };
    match stage {
        Stage::Broadcast => state,
        Stage::Receiver => state.saturating_add(STATES),
        Stage::Serverbound => state.saturating_add(STATES).saturating_add(STATES),
    }
}

const fn stage_index(stage: Stage) -> usize {
    match stage {
        Stage::Broadcast => 0,
        Stage::Receiver => 1,
        Stage::Serverbound => 2,
    }
}

impl Mask {
    /// Marks one packet type; ids the mask cannot hold are ignored.
    pub fn set(&mut self, stage: Stage, state: State, id: i32) {
        let Ok(id) = usize::try_from(id) else {
            return;
        };
        if let Some(word) = self
            .bits
            .get_mut(table(stage, state))
            .and_then(|t| t.get_mut(id / 64))
        {
            *word |= 1u64 << (id % 64);
        }
    }

    /// Marks every packet type of a state.
    pub fn set_all(&mut self, stage: Stage, state: State) {
        if let Some(t) = self.bits.get_mut(table(stage, state)) {
            *t = [u64::MAX; WORDS];
        }
    }
}

/// What the encoder of a batch found out about its packets for the receiver stage
/// ([`PacketTap::batch_mark`]): a writer passes a marked batch on without looking into it as
/// long as the listeners are the ones the mark was made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BatchMark(u64);

impl BatchMark {
    /// Nothing known: the writer looks for packet types with listeners in the batch.
    pub const UNKNOWN: Self = Self(0);
}

/// The outcome of the broadcast stage, kept with a [`SharedPacket`].
#[derive(Debug)]
pub(crate) struct Tapped {
    /// [`PacketTap::generation`] of the listeners that decided.
    generation: u64,
    verdict: Verdict<Arc<SharedPacket>>,
}

/// The hook of one server. Without an [`Interceptor`], or without listeners, every question
/// is answered by the mask.
pub struct PacketTap {
    mask: [[AtomicU64; WORDS]; TABLES],
    /// Whether any packet type has a listener of a stage ([`stage_index`]).
    active: [AtomicBool; 3],
    /// Whether any clientbound packet type has a listener, of either stage.
    clientbound: AtomicBool,
    /// Changes with the listeners.
    generation: AtomicU64,
    /// Numbers connections.
    connections: AtomicU64,
    interceptor: OnceLock<Arc<dyn Interceptor>>,
}

impl std::fmt::Debug for PacketTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacketTap")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Default for PacketTap {
    fn default() -> Self {
        Self {
            mask: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            active: std::array::from_fn(|_| AtomicBool::new(false)),
            clientbound: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            connections: AtomicU64::new(0),
            interceptor: OnceLock::new(),
        }
    }
}

/// The packet id a body starts with.
pub fn packet_id(body: &[u8]) -> Option<i32> {
    Reader::new(body).varint().ok()
}

impl PacketTap {
    /// A tap without listeners.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the interceptor; the first one stays.
    pub fn install(&self, interceptor: Arc<dyn Interceptor>) {
        let _ = self.interceptor.set(interceptor);
    }

    /// Takes over the packet types with listeners. Words are replaced one by one, so a type
    /// that keeps its listeners is never unmarked in between.
    pub fn store(&self, mask: &Mask) {
        let mut active = [false; 3];
        for (index, (words, new)) in self.mask.iter().zip(&mask.bits).enumerate() {
            for (word, new) in words.iter().zip(new) {
                word.store(*new, Ordering::Relaxed);
            }
            if new.iter().any(|w| *w != 0)
                && let Some(a) = active.get_mut(index.checked_div(STATES).unwrap_or(0))
            {
                *a = true;
            }
        }
        let [broadcast, receiver, _] = active;
        self.clientbound
            .store(broadcast || receiver, Ordering::Relaxed);
        for (flag, on) in self.active.iter().zip(active) {
            flag.store(on, Ordering::Relaxed);
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// A number for a new connection.
    pub fn next_connection(&self) -> u64 {
        self.connections
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }

    /// Whether any packet type of `direction` has a listener: the one load and branch a
    /// packet costs without listeners.
    #[inline]
    pub fn active(&self, direction: Direction) -> bool {
        match direction {
            Direction::Clientbound => self.clientbound.load(Ordering::Relaxed),
            Direction::Serverbound => self.listens(Stage::Serverbound),
        }
    }

    /// Whether any packet type has a listener of `stage`.
    #[inline]
    pub fn listens(&self, stage: Stage) -> bool {
        self.active
            .get(stage_index(stage))
            .is_some_and(|a| a.load(Ordering::Relaxed))
    }

    /// Whether a packet type has a listener of `stage`.
    #[inline]
    pub fn wants_at(&self, stage: Stage, state: State, id: i32) -> bool {
        let Ok(id) = usize::try_from(id) else {
            return false;
        };
        self.mask
            .get(table(stage, state))
            .and_then(|t| t.get(id / 64))
            .is_some_and(|w| w.load(Ordering::Relaxed) & (1u64 << (id % 64)) != 0)
    }

    /// Whether a packet type has a listener of any stage.
    #[inline]
    pub fn wants(&self, direction: Direction, state: State, id: i32) -> bool {
        match direction {
            Direction::Clientbound => {
                self.wants_at(Stage::Broadcast, state, id)
                    || self.wants_at(Stage::Receiver, state, id)
            }
            Direction::Serverbound => self.wants_at(Stage::Serverbound, state, id),
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// A mark for a batch none of whose packet types has a listener per receiver: take it
    /// before asking [`PacketTap::wants_at`] about the batch's packets, and send the batch
    /// with [`BatchMark::UNKNOWN`] instead if one of them has.
    pub fn batch_mark(&self) -> BatchMark {
        BatchMark(self.generation().wrapping_add(1))
    }

    /// Whether the receiver stage can pass a batch on without looking into it: its encoder
    /// found no packet type with a listener per receiver, and the listeners did not change
    /// since.
    #[inline]
    pub fn skips(&self, mark: BatchMark) -> bool {
        mark != BatchMark::UNKNOWN && mark.0 == self.generation().wrapping_add(1)
    }

    /// The broadcast stage of a shared packet: run by the first sender, kept with the packet
    /// and reused for every other receiver. A packet that outlives a change of the listeners
    /// (a cached chunk) runs the stage on every send from then on.
    pub fn shared(&self, state: State, packet: &Arc<SharedPacket>) -> Verdict<Arc<SharedPacket>> {
        let Some(id) = packet.id() else {
            return Verdict::Pass;
        };
        if !self.wants_at(Stage::Broadcast, state, id) {
            return Verdict::Pass;
        }
        let Some(interceptor) = self.interceptor.get() else {
            return Verdict::Pass;
        };
        let generation = self.generation();
        let run = || match interceptor.broadcast(state, id, packet.body()) {
            Verdict::Pass => Verdict::Pass,
            Verdict::Drop => Verdict::Drop,
            Verdict::Replace(body) => Verdict::Replace(Arc::new(SharedPacket::new(body))),
        };
        let tapped = packet.tapped().get_or_init(|| {
            Box::new(Tapped {
                generation,
                verdict: run(),
            })
        });
        if tapped.generation == generation {
            tapped.verdict.clone()
        } else {
            run()
        }
    }

    /// The listeners of a clientbound packet for one receiver; `broadcast` also runs the
    /// broadcast stage first (a body that is not a [`SharedPacket`] and did not pass that
    /// stage where it was encoded).
    pub fn clientbound(
        &self,
        peer: Peer,
        state: State,
        body: &[u8],
        broadcast: bool,
    ) -> Verdict<Vec<u8>> {
        let Some(id) = packet_id(body) else {
            return Verdict::Pass;
        };
        let Some(interceptor) = self.interceptor.get() else {
            return Verdict::Pass;
        };
        let receiver = |id: i32, body: &[u8]| {
            if self.wants_at(Stage::Receiver, state, id) {
                interceptor.clientbound(peer, state, id, body)
            } else {
                Verdict::Pass
            }
        };
        if !broadcast || !self.wants_at(Stage::Broadcast, state, id) {
            return receiver(id, body);
        }
        match interceptor.broadcast(state, id, body) {
            Verdict::Pass => receiver(id, body),
            Verdict::Drop => Verdict::Drop,
            Verdict::Replace(replaced) => {
                // A listener may have changed the packet's type.
                let id = packet_id(&replaced).unwrap_or(id);
                match receiver(id, &replaced) {
                    Verdict::Pass => Verdict::Replace(replaced),
                    other => other,
                }
            }
        }
    }

    /// The listeners of a serverbound packet.
    pub fn serverbound(&self, peer: Peer, state: State, body: &[u8]) -> Verdict<Vec<u8>> {
        let Some(id) = packet_id(body) else {
            return Verdict::Pass;
        };
        if !self.wants_at(Stage::Serverbound, state, id) {
            return Verdict::Pass;
        }
        match self.interceptor.get() {
            Some(interceptor) => interceptor.serverbound(peer, state, id, body),
            None => Verdict::Pass,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Appends a byte per stage and counts the broadcast stage.
    #[derive(Default)]
    struct Marker {
        broadcasts: AtomicUsize,
    }

    impl Interceptor for Marker {
        fn broadcast(&self, _: State, _: i32, body: &[u8]) -> Verdict<Vec<u8>> {
            self.broadcasts.fetch_add(1, Ordering::Relaxed);
            let mut body = body.to_vec();
            body.push(0xb0);
            Verdict::Replace(body)
        }
        fn clientbound(&self, peer: Peer, _: State, _: i32, body: &[u8]) -> Verdict<Vec<u8>> {
            match peer.player {
                1 => Verdict::Pass,
                2 => Verdict::Drop,
                _ => {
                    let mut body = body.to_vec();
                    body.push(0xc0);
                    Verdict::Replace(body)
                }
            }
        }
        fn serverbound(&self, _: Peer, _: State, _: i32, _: &[u8]) -> Verdict<Vec<u8>> {
            Verdict::Drop
        }
    }

    fn peer(player: u128) -> Peer {
        Peer {
            connection: 1,
            player,
        }
    }

    #[test]
    fn nothing_is_wanted_without_listeners() {
        let tap = PacketTap::new();
        tap.install(Arc::new(Marker::default()));
        assert!(!tap.active(Direction::Clientbound));
        assert!(!tap.wants(Direction::Clientbound, State::Play, 7));
        let packet = Arc::new(SharedPacket::new(vec![7, 1]));
        assert!(matches!(tap.shared(State::Play, &packet), Verdict::Pass));
        assert_eq!(
            tap.clientbound(peer(3), State::Play, &[7, 1], true),
            Verdict::Pass
        );
        assert_eq!(
            tap.serverbound(peer(3), State::Play, &[7, 1]),
            Verdict::Pass
        );
    }

    #[test]
    fn the_broadcast_stage_runs_once_per_shared_packet() {
        let marker = Arc::new(Marker::default());
        let tap = PacketTap::new();
        let interceptor: Arc<dyn Interceptor> = marker.clone();
        tap.install(interceptor);
        let mut mask = Mask::default();
        mask.set(Stage::Broadcast, State::Play, 7);
        mask.set(Stage::Receiver, State::Play, 7);
        tap.store(&mask);
        assert!(tap.active(Direction::Clientbound));
        assert!(tap.listens(Stage::Receiver) && tap.listens(Stage::Broadcast));
        assert!(!tap.active(Direction::Serverbound));
        assert!(!tap.wants(Direction::Clientbound, State::Play, 8));
        assert!(!tap.wants(Direction::Clientbound, State::Configuration, 7));

        let packet = Arc::new(SharedPacket::new(vec![7, 1]));
        let first = tap.shared(State::Play, &packet);
        let second = tap.shared(State::Play, &packet);
        let (Verdict::Replace(a), Verdict::Replace(b)) = (first, second) else {
            panic!("the broadcast stage replaces the packet");
        };
        assert!(Arc::ptr_eq(&a, &b), "all receivers share the replacement");
        assert_eq!(a.body(), [7, 1, 0xb0]);
        assert_eq!(marker.broadcasts.load(Ordering::Relaxed), 1);

        // The receiver stage: untouched, cancelled, and a copy for one receiver only.
        assert_eq!(
            tap.clientbound(peer(1), State::Play, a.body(), false),
            Verdict::Pass
        );
        assert_eq!(
            tap.clientbound(peer(2), State::Play, a.body(), false),
            Verdict::Drop
        );
        assert_eq!(
            tap.clientbound(peer(3), State::Play, a.body(), false),
            Verdict::Replace(vec![7, 1, 0xb0, 0xc0])
        );
        // A body that is not shared passes both stages.
        assert_eq!(
            tap.clientbound(peer(1), State::Play, &[7, 1], true),
            Verdict::Replace(vec![7, 1, 0xb0])
        );

        // New listeners: the kept outcome is not reused.
        tap.store(&mask);
        assert!(matches!(
            tap.shared(State::Play, &packet),
            Verdict::Replace(_)
        ));
        assert_eq!(marker.broadcasts.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_batch_mark_holds_until_the_listeners_change() {
        let tap = PacketTap::new();
        let mark = tap.batch_mark();
        assert!(tap.skips(mark));
        assert!(!tap.skips(BatchMark::UNKNOWN));
        tap.store(&Mask::default());
        assert!(!tap.skips(mark));
        assert!(tap.skips(tap.batch_mark()));
    }

    #[test]
    fn serverbound_packets_have_their_own_mask() {
        let tap = PacketTap::new();
        tap.install(Arc::new(Marker::default()));
        let mut mask = Mask::default();
        mask.set_all(Stage::Serverbound, State::Play);
        tap.store(&mask);
        assert!(tap.active(Direction::Serverbound));
        assert!(!tap.active(Direction::Clientbound));
        assert!(!tap.listens(Stage::Receiver));
        assert_eq!(
            tap.serverbound(peer(1), State::Play, &[200, 1]),
            Verdict::Drop
        );
        assert_eq!(tap.serverbound(peer(1), State::Login, &[0]), Verdict::Pass);
    }
}
