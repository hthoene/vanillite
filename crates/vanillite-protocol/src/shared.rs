//! Packets encoded once for many receivers, and the inbound play packet envelope.

use std::collections::VecDeque;
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use crate::Packet;
use crate::frame::encode_body;
use crate::sets::ServerboundPlay;

/// A packet encoded once for many receivers (broadcasts, chunk packets, configuration data).
/// Its compressed frame is built by the first writer that needs it and reused by all others
/// with the same compression threshold. Clients of an older version (multi-version layer) get
/// a translated body, likewise built once per version by the first writer that needs it (up to
/// [`TRANSLATIONS`] versions; the slots are allocated by the first translation).
///
/// A packet kept for long (a cached chunk packet) can be replaced by its [`compacted`] copy,
/// which holds only the frame: its body is inflated from the frame by the first caller that
/// needs it (another threshold, a translation, a packet listener) and kept from then on.
///
/// [`compacted`]: SharedPacket::compacted
#[derive(Debug)]
pub struct SharedPacket {
    /// The encoded body; empty until first needed in a [`SharedPacket::compacted`] copy.
    body: OnceLock<Vec<u8>>,
    /// Length of the body.
    len: usize,
    /// Packet id at the start of the body.
    id: Option<i32>,
    /// Made by [`SharedPacket::compacted`].
    compacted: bool,
    frame: OnceLock<(Option<usize>, Vec<u8>)>,
    translated: OnceLock<Box<[Translation; TRANSLATIONS]>>,
    /// What the broadcast listeners of the packet tap decided (allocated by the first
    /// listener that sees the packet).
    tapped: OnceLock<Box<crate::tap::Tapped>>,
}

/// How many client versions one shared packet caches translations for.
pub const TRANSLATIONS: usize = 12;

type Translation = OnceLock<(i32, Option<Box<[u8]>>)>;

impl SharedPacket {
    /// Wraps an encoded body (`id` + fields).
    pub fn new(body: Vec<u8>) -> Self {
        Self {
            len: body.len(),
            id: crate::tap::packet_id(&body),
            body: OnceLock::from(body),
            compacted: false,
            frame: OnceLock::new(),
            translated: OnceLock::new(),
            tapped: OnceLock::new(),
        }
    }

    pub(crate) fn tapped(&self) -> &OnceLock<Box<crate::tap::Tapped>> {
        &self.tapped
    }

    /// Encodes a packet.
    pub fn of<P: Packet>(packet: &P) -> Arc<Self> {
        Arc::new(Self::new(encode_body(packet)))
    }

    /// The encoded body.
    pub fn body(&self) -> &[u8] {
        self.body.get_or_init(|| {
            let mut body = Vec::new();
            if let Some((threshold, frame)) = self.frame.get() {
                let _ = crate::frame::for_each_body(frame, *threshold, |b| {
                    body.extend_from_slice(b);
                    Ok(false)
                });
            }
            body
        })
    }

    /// Length of the body.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the body is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The packet id (the first field of the body).
    pub fn id(&self) -> Option<i32> {
        self.id
    }

    /// A copy that holds only the frame, once a writer built it (`None` before, for an
    /// unencodable body and for a copy that is compacted already): for a cache that keeps
    /// the packet long after its first receivers.
    pub fn compacted(&self) -> Option<Self> {
        if self.compacted {
            return None;
        }
        let (threshold, frame) = self.frame.get()?;
        if frame.is_empty() {
            return None;
        }
        Some(Self {
            body: OnceLock::new(),
            len: self.len,
            id: self.id,
            compacted: true,
            frame: OnceLock::from((*threshold, frame.clone())),
            translated: OnceLock::new(),
            tapped: OnceLock::new(),
        })
    }

    /// The cached frame for `threshold`, built with `build` by the first caller. Returns
    /// `None` when the cache holds a frame for another threshold.
    pub fn frame(
        &self,
        threshold: Option<usize>,
        build: impl FnOnce(&[u8]) -> Vec<u8>,
    ) -> Option<&[u8]> {
        let (t, frame) = self.frame.get_or_init(|| (threshold, build(self.body())));
        (*t == threshold).then_some(frame.as_slice())
    }

    /// The cached translation of the body for clients of protocol `version`, built with
    /// `build` by the first caller (`None` = the packet is dropped for them). Returns `None`
    /// when the cache is full with the translations for other versions.
    pub fn translated(
        &self,
        version: i32,
        build: impl FnOnce(&[u8]) -> Option<Vec<u8>>,
    ) -> Option<Option<&[u8]>> {
        let slots = self
            .translated
            .get_or_init(|| Box::new(std::array::from_fn(|_| OnceLock::new())));
        let mut build = Some(build);
        for slot in slots.iter() {
            // Only the caller that fills an empty slot builds, so a slot of another version
            // leaves `build` for the next one.
            let (v, body) = slot.get_or_init(|| {
                let body = build.take().and_then(|b| b(self.body()));
                (version, body.map(Vec::into_boxed_slice))
            });
            if *v == version {
                return Some(body.as_deref());
            }
        }
        None
    }
}

/// A validated serverbound play packet with its sequence number on its connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Inbound {
    pub seq: u64,
    pub packet: ServerboundPlay,
}

/// Creates the queue of one connection's play packets for the world. At most `max` packets
/// wait (SEC-17); the memory follows the packets that wait, not `max`, so thousands of idle
/// connections stay small. The network side locks the queue once per packet, the world once
/// per [`InboundReceiver::take`]: no shared counters are touched per packet by both sides.
pub fn inbound_queue(max: usize) -> (InboundSender, InboundReceiver) {
    let queue = Arc::new(Mutex::new(InboundState::default()));
    (
        InboundSender {
            queue: Arc::clone(&queue),
            max,
        },
        InboundReceiver { queue },
    )
}

/// The packets waiting in an [`inbound_queue`] and which side is gone.
#[derive(Debug, Default)]
struct InboundState {
    packets: VecDeque<Inbound>,
    sender_gone: bool,
    receiver_gone: bool,
}

/// Capacity an empty queue keeps after a burst; larger ones are released.
const IDLE_CAPACITY: usize = 64;

fn lock(queue: &Mutex<InboundState>) -> MutexGuard<'_, InboundState> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The network side of an [`inbound_queue`].
#[derive(Debug)]
pub struct InboundSender {
    queue: Arc<Mutex<InboundState>>,
    max: usize,
}

impl InboundSender {
    /// Queues a packet. Returns false (and drops it) if `max` packets wait or the receiver is
    /// gone.
    pub fn try_send(&self, inbound: Inbound) -> bool {
        let mut queue = lock(&self.queue);
        if queue.receiver_gone || queue.packets.len() >= self.max {
            return false;
        }
        queue.packets.push_back(inbound);
        true
    }
}

impl Drop for InboundSender {
    fn drop(&mut self) {
        lock(&self.queue).sender_gone = true;
    }
}

/// The world side of an [`inbound_queue`].
#[derive(Debug)]
pub struct InboundReceiver {
    queue: Arc<Mutex<InboundState>>,
}

impl InboundReceiver {
    /// The next packet, like [`std::sync::mpsc::Receiver::try_recv`].
    pub fn try_recv(&self) -> Result<Inbound, TryRecvError> {
        let mut queue = lock(&self.queue);
        match queue.packets.pop_front() {
            Some(inbound) => Ok(inbound),
            None if queue.sender_gone => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }

    /// Moves up to `max` waiting packets to `out` in arrival order, with one lock, like `max`
    /// calls of [`Self::try_recv`]: fails with [`TryRecvError::Disconnected`] (after filling
    /// `out`) if fewer than `max` waited and the sender is gone.
    pub fn take(&self, max: usize, out: &mut Vec<Inbound>) -> Result<(), TryRecvError> {
        let mut queue = lock(&self.queue);
        let n = queue.packets.len().min(max);
        out.extend(queue.packets.drain(..n));
        if queue.packets.is_empty() && queue.packets.capacity() > IDLE_CAPACITY {
            queue.packets.shrink_to(IDLE_CAPACITY);
        }
        if n < max && queue.sender_gone {
            return Err(TryRecvError::Disconnected);
        }
        Ok(())
    }
}

impl Drop for InboundReceiver {
    fn drop(&mut self) {
        let mut queue = lock(&self.queue);
        queue.receiver_gone = true;
        queue.packets = VecDeque::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn compacted_packet_keeps_frame_and_inflates_body() {
        let body: Vec<u8> = std::iter::once(0x27)
            .chain((0..4000u32).map(|i| i.to_le_bytes()[0]))
            .collect();
        for threshold in [None, Some(256)] {
            let packet = SharedPacket::new(body.clone());
            assert!(packet.compacted().is_none());
            let mut codec = crate::compress::Codec::default();
            let frame = packet
                .frame(threshold, |b| {
                    let mut f = Vec::new();
                    crate::frame::encode_frame(b, threshold, &mut codec, &mut f).unwrap();
                    f
                })
                .unwrap()
                .to_vec();
            let compacted = packet.compacted().unwrap();
            assert!(compacted.compacted().is_none());
            assert_eq!((compacted.id(), compacted.len()), (Some(0x27), body.len()));
            assert_eq!(
                compacted.frame(threshold, |_| unreachable!()),
                Some(&frame[..])
            );
            assert_eq!(compacted.body(), &body[..]);
        }
    }

    fn packet(seq: u64) -> Inbound {
        Inbound {
            seq,
            packet: ServerboundPlay::Unhandled { id: 0 },
        }
    }

    #[test]
    fn inbound_queue_never_holds_more_than_max() {
        let (tx, rx) = inbound_queue(2);
        assert!(tx.try_send(packet(1)));
        assert!(tx.try_send(packet(2)));
        assert!(!tx.try_send(packet(3)));
        assert_eq!(rx.try_recv().map(|i| i.seq), Ok(1));
        assert!(tx.try_send(packet(4)));
        assert!(!tx.try_send(packet(5)));
        assert_eq!(rx.try_recv().map(|i| i.seq), Ok(2));
        assert_eq!(rx.try_recv().map(|i| i.seq), Ok(4));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn inbound_queue_takes_in_order_up_to_max() {
        let (tx, rx) = inbound_queue(8);
        for seq in 1..=5 {
            assert!(tx.try_send(packet(seq)));
        }
        let mut out = Vec::new();
        assert_eq!(rx.take(3, &mut out), Ok(()));
        assert_eq!(out.iter().map(|i| i.seq).collect::<Vec<_>>(), [1, 2, 3]);
        drop(tx);
        out.clear();
        assert_eq!(rx.take(2, &mut out), Ok(()));
        assert_eq!(out.iter().map(|i| i.seq).collect::<Vec<_>>(), [4, 5]);
        assert_eq!(rx.take(8, &mut out), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn inbound_queue_reports_disconnect() {
        let (tx, rx) = inbound_queue(4);
        assert!(tx.try_send(packet(1)));
        drop(tx);
        assert_eq!(rx.try_recv().map(|i| i.seq), Ok(1));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));

        let (tx, rx) = inbound_queue(4);
        drop(rx);
        assert!(!tx.try_send(packet(1)));
    }
}
