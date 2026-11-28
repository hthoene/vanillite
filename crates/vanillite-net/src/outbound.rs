//! The outbound side of a connection: an unbounded channel bounded by bytes (SEC-17) and the
//! writer task that frames, compresses, encrypts and writes with one syscall per batch.
//!
//! A connection starts out writing whatever is queued as soon as it can. Once its owner calls
//! [`Outbound::flush`] (a world at the end of each tick), the writer waits for the flushes
//! instead: one wake-up and one write per tick, not one per packet.

use crate::sent_log::SentLog;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, mpsc};
use vanillite_multiversion::ClientboundHalf;
use vanillite_protocol::compress::Codec;
use vanillite_protocol::frame::{
    FrameEncoder, encode_body, encode_frame, for_each_body, for_each_id,
};
use vanillite_protocol::tap::{BatchMark, PacketTap, Peer, Stage, Verdict};
use vanillite_protocol::{Direction, Packet, SharedPacket, State};

/// A message to a connection's writer.
#[derive(Debug)]
pub(crate) enum Out {
    /// An encoded packet body owned by this connection.
    Body(Vec<u8>),
    /// A shared packet.
    Shared(Arc<SharedPacket>),
    /// Frames built with [`encode_frame`] for a compression threshold, and what their
    /// encoder knows about their packet types (for the packet tap).
    Batch(Vec<u8>, Option<usize>, BatchMark),
    /// An encoded packet body the listeners of the packet tap do not see.
    Untapped(Vec<u8>),
    /// Compression on from here (sent after `login_compression`).
    Compression(i32),
    /// Encryption on from here (after the key exchange).
    Encryption([u8; 16]),
    /// Translate every packet from here for a client of an older version.
    Adapter(Box<ClientboundHalf>),
    /// The protocol state of the packets from here (for the packet tap).
    State(State),
    /// The player this connection belongs to (for the packet tap).
    Player(u128),
    /// Flush and close the connection.
    Close,
}

impl Out {
    /// The bytes this message counts against the connection's outbound limit.
    fn queued(&self) -> usize {
        match self {
            Out::Body(body) | Out::Untapped(body) => body.len(),
            Out::Shared(packet) => packet.len(),
            Out::Batch(frames, ..) => frames.len(),
            _ => 0,
        }
    }
}

/// Sends packets to one connection. Cheap to clone; usable from any thread.
#[derive(Debug, Clone)]
pub struct Outbound {
    tx: mpsc::UnboundedSender<Out>,
    queued: Arc<AtomicUsize>,
    max_bytes: usize,
    closed: Arc<AtomicBool>,
    /// The last compression threshold queued ([`NO_COMPRESSION`] = off).
    threshold: Arc<AtomicUsize>,
    /// The last packets the writer wrote.
    sent: Arc<SentLog>,
    /// The connection's number for the packet tap.
    connection: u64,
    /// Writes wait for [`Outbound::flush`].
    flushes: Arc<Flushes>,
}

/// The writer's side of [`Outbound::flush`].
#[derive(Debug, Default)]
pub(crate) struct Flushes {
    /// Set by the first flush: from then on the writer waits for flushes.
    deferred: AtomicBool,
    notify: Notify,
}

/// [`Outbound::threshold`] value for compression off.
const NO_COMPRESSION: usize = usize::MAX;
/// Queued bytes that start a write without waiting for the flush (the chunks of a join).
const FLUSH_BYTES: usize = 128 * 1024;
/// The longest a deferred write waits for a flush (a player between worlds, a stalled tick).
const FLUSH_FALLBACK: Duration = Duration::from_millis(250);

impl Outbound {
    pub(crate) fn new(max_bytes: usize, connection: u64) -> (Self, OutReceiver, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let flushes = Arc::new(Flushes::default());
        let rx = OutReceiver {
            rx,
            closed: Arc::clone(&closed),
            flushes: Arc::clone(&flushes),
        };
        (
            Self {
                tx,
                queued: Arc::clone(&queued),
                max_bytes,
                closed,
                threshold: Arc::new(AtomicUsize::new(NO_COMPRESSION)),
                sent: Arc::new(SentLog::new()),
                connection,
                flushes,
            },
            rx,
            queued,
        )
    }

    fn push(&self, msg: Out, bytes: usize) -> bool {
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        let total = self
            .queued
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        if total > self.max_bytes {
            // The client does not read fast enough: drop it instead of buffering (SEC-17).
            self.close();
            return false;
        }
        let sent = self.tx.send(msg).is_ok();
        if total >= FLUSH_BYTES && self.flushes.deferred.load(Ordering::Relaxed) {
            self.flushes.notify.notify_one();
        }
        sent
    }

    /// Writes what is queued; from the first call on, the connection writes only on these
    /// calls (and when much is queued, or after [`FLUSH_FALLBACK`]). Its owner calls this once
    /// per tick, after the tick's packets.
    pub fn flush(&self) {
        self.flushes.deferred.store(true, Ordering::Relaxed);
        if self.queued.load(Ordering::Relaxed) > 0 {
            self.flushes.notify.notify_one();
        }
    }

    /// Queues a typed packet. Returns false if the connection is closed or overloaded.
    pub fn send<P: Packet>(&self, packet: &P) -> bool {
        let body = encode_body(packet);
        let n = body.len();
        self.push(Out::Body(body), n)
    }

    /// Queues an encoded body.
    pub fn send_body(&self, body: Vec<u8>) -> bool {
        let n = body.len();
        self.push(Out::Body(body), n)
    }

    /// Queues an encoded body that the listeners of the packet tap do not see (a packet a
    /// plugin sends past them).
    pub fn send_untapped(&self, body: Vec<u8>) -> bool {
        let n = body.len();
        self.push(Out::Untapped(body), n)
    }

    /// The connection's number for the packet tap.
    pub(crate) fn connection(&self) -> u64 {
        self.connection
    }

    /// Queues many packets as one entry: frames built with [`encode_frame`] for `threshold`
    /// (best this connection's, [`Outbound::threshold`]). The packets of a batch pass the
    /// receiver stage of the packet tap only: whoever encodes them for many receivers runs
    /// the broadcast stage there ([`PacketTap::shared`]). `mark` lets the writer skip looking
    /// into the batch ([`PacketTap::batch_mark`]); [`BatchMark::UNKNOWN`] is always right.
    pub fn send_batch(&self, frames: Vec<u8>, threshold: Option<usize>, mark: BatchMark) -> bool {
        let n = frames.len();
        self.push(Out::Batch(frames, threshold, mark), n)
    }

    /// The compression threshold the connection's writer uses once everything queued so far
    /// is written.
    pub fn threshold(&self) -> Option<usize> {
        let t = self.threshold.load(Ordering::Relaxed);
        (t != NO_COMPRESSION).then_some(t)
    }

    /// Queues a shared packet.
    pub fn send_shared(&self, packet: &Arc<SharedPacket>) -> bool {
        self.push(Out::Shared(Arc::clone(packet)), packet.len())
    }

    pub(crate) fn control(&self, msg: Out) -> bool {
        if let Out::Compression(t) = msg {
            let t = usize::try_from(t).unwrap_or(NO_COMPRESSION);
            self.threshold.store(t, Ordering::Relaxed);
        }
        self.push(msg, 0)
    }

    /// Flushes what is queued and closes the connection.
    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::Relaxed) {
            let _ = self.tx.send(Out::Close);
            self.flushes.notify.notify_one();
        }
    }

    /// The record of the last packets written, shared with the writer.
    pub(crate) fn sent_log(&self) -> Arc<SentLog> {
        Arc::clone(&self.sent)
    }

    /// Whether the connection was closed (by [`Outbound::close`] or its writer ending).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// The writer's end of an [`Outbound`]. Dropping it (the writer ended) closes the connection
/// for the senders, so [`Outbound::is_closed`] reads one flag instead of the channel state
/// the writer changes with every message.
#[derive(Debug)]
pub(crate) struct OutReceiver {
    rx: mpsc::UnboundedReceiver<Out>,
    closed: Arc<AtomicBool>,
    flushes: Arc<Flushes>,
}

impl OutReceiver {
    pub(crate) async fn recv(&mut self) -> Option<Out> {
        self.rx.recv().await
    }

    /// The next message to write: at once until the first [`Outbound::flush`], then after
    /// a flush (or [`FLUSH_FALLBACK`]). `None`: every sender is gone. With `idle`, returns
    /// [`Next::Idle`] once nothing came for [`IDLE_RELEASE`] (the writer then releases its
    /// buffers).
    async fn next_write(&mut self, idle: bool) -> Next {
        let mut waited = Duration::ZERO;
        loop {
            if !self.flushes.deferred.load(Ordering::Relaxed) {
                return self.rx.recv().await.map_or(Next::Gone, Next::Msg);
            }
            let _ = tokio::time::timeout(FLUSH_FALLBACK, self.flushes.notify.notified()).await;
            match self.rx.try_recv() {
                Ok(msg) => return Next::Msg(msg),
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => return Next::Gone,
            }
            waited = waited.saturating_add(FLUSH_FALLBACK);
            if idle && waited >= IDLE_RELEASE {
                return Next::Idle;
            }
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<Out, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for OutReceiver {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

/// Capacity of a writer's buffers kept after a write; larger ones are released.
const IDLE_BUFFER_MAX: usize = 64 * 1024;
/// A writer that wrote nothing for this long releases its buffers entirely.
const IDLE_RELEASE: Duration = Duration::from_secs(1);

/// What [`OutReceiver::next_write`] got.
enum Next {
    Msg(Out),
    /// Every sender is gone.
    Gone,
    /// Nothing to write for [`IDLE_RELEASE`].
    Idle,
}

/// A writer's side of the packet tap: listeners see a connection's packets in the order they
/// are written, before any translation for other client versions.
struct OutTap {
    tap: Arc<PacketTap>,
    peer: Peer,
    state: State,
}

impl OutTap {
    fn new(tap: Arc<PacketTap>, connection: u64) -> Self {
        Self {
            tap,
            peer: Peer {
                connection,
                player: 0,
            },
            state: State::Handshake,
        }
    }

    /// Runs the listeners on a message unless no clientbound packet type has one (one load
    /// and branch). Returns what is left to write.
    #[inline]
    fn filter(&mut self, msg: Out, codec: &mut Codec) -> Option<Out> {
        match msg {
            Out::State(state) => {
                self.state = state;
                None
            }
            Out::Player(player) => {
                self.peer.player = player;
                None
            }
            msg if self.tap.active(Direction::Clientbound) => self.apply(msg, codec),
            msg => Some(msg),
        }
    }

    #[cold]
    fn apply(&self, msg: Out, codec: &mut Codec) -> Option<Out> {
        match msg {
            Out::Body(body) => match self.tap.clientbound(self.peer, self.state, &body, true) {
                Verdict::Pass => Some(Out::Body(body)),
                Verdict::Drop => None,
                Verdict::Replace(body) => Some(Out::Body(body)),
            },
            Out::Shared(packet) => {
                let packet = match self.tap.shared(self.state, &packet) {
                    Verdict::Pass => packet,
                    Verdict::Drop => return None,
                    Verdict::Replace(replaced) => replaced,
                };
                match self
                    .tap
                    .clientbound(self.peer, self.state, packet.body(), false)
                {
                    // Still the buffer every receiver shares.
                    Verdict::Pass => Some(Out::Shared(packet)),
                    Verdict::Drop => None,
                    Verdict::Replace(body) => Some(Out::Body(body)),
                }
            }
            Out::Batch(frames, threshold, mark) => self.batch(frames, threshold, mark, codec),
            other => Some(other),
        }
    }

    /// A batch stays as it is unless it holds a packet type with a listener per receiver;
    /// then it is framed anew with the listeners' changes. (The broadcast stage ran where
    /// the batch was encoded.) A batch whose encoder found no such type is not looked into.
    fn batch(
        &self,
        frames: Vec<u8>,
        threshold: Option<usize>,
        mark: BatchMark,
        codec: &mut Codec,
    ) -> Option<Out> {
        if !self.tap.listens(Stage::Receiver) || self.tap.skips(mark) {
            return Some(Out::Batch(frames, threshold, mark));
        }
        let mut wanted = false;
        let walked = for_each_id(&frames, threshold, |id| {
            // A compressed frame hides its id: look inside.
            wanted = id.is_none_or(|id| self.tap.wants_at(Stage::Receiver, self.state, id));
            !wanted
        });
        if walked.is_err() || !wanted {
            return Some(Out::Batch(frames, threshold, mark));
        }
        let mut out = Vec::with_capacity(frames.len());
        let rebuilt = for_each_body(&frames, threshold, |body| {
            match self.tap.clientbound(self.peer, self.state, body, false) {
                Verdict::Pass => encode_frame(body, threshold, codec, &mut out)?,
                Verdict::Drop => {}
                Verdict::Replace(body) => encode_frame(&body, threshold, codec, &mut out)?,
            }
            Ok(true)
        });
        match rebuilt {
            Ok(()) if out.is_empty() => None,
            Ok(()) => Some(Out::Batch(out, threshold, BatchMark::UNKNOWN)),
            Err(_) => Some(Out::Batch(frames, threshold, mark)),
        }
    }
}

/// The wire side of a TCP writer: framing, compression, encryption and the translation for
/// clients of an older version.
struct Wire {
    encoder: FrameEncoder,
    codec: Codec,
    buf: Vec<u8>,
    adapter: Option<Box<ClientboundHalf>>,
}

impl Wire {
    /// Appends a message to the write buffer. Returns false when the connection is to close.
    fn write(&mut self, msg: Out) -> bool {
        let Self {
            encoder,
            codec,
            buf,
            adapter,
        } = self;
        match msg {
            Out::Body(body) | Out::Untapped(body) => match adapter.as_mut() {
                Some(a) => {
                    let mut open = match a.translate(&body) {
                        Ok(Some(t)) => encoder.body(t, buf).is_ok(),
                        Ok(None) => true,
                        Err(e) => {
                            log::debug!("multiversion: untranslatable packet: {e}");
                            true
                        }
                    };
                    while let Some(t) = a.pending() {
                        open &= encoder.body(t, buf).is_ok();
                    }
                    open
                }
                None => encoder.body(&body, buf).is_ok(),
            },
            Out::Shared(p) => match adapter.as_mut() {
                Some(a) => {
                    let mut open = match a.translate_shared(&p) {
                        Some(t) => encoder.body(t, buf).is_ok(),
                        None => true,
                    };
                    while let Some(t) = a.pending() {
                        open &= encoder.body(t, buf).is_ok();
                    }
                    open
                }
                None => {
                    let threshold = encoder.threshold();
                    let cached = p.frame(threshold, |body| {
                        let mut f = Vec::new();
                        // An unencodable body yields an empty frame that is skipped below.
                        let _ = encode_frame(body, threshold, codec, &mut f);
                        f
                    });
                    match cached {
                        Some(frame) if !frame.is_empty() => {
                            encoder.frame(frame, buf);
                            true
                        }
                        _ => encoder.body(p.body(), buf).is_ok(),
                    }
                }
            },
            Out::Batch(frames, threshold, _) => match adapter.as_mut() {
                Some(a) => for_each_body(&frames, threshold, |body| {
                    match a.translate(body) {
                        Ok(Some(t)) => encoder.body(t, buf)?,
                        Ok(None) => {}
                        Err(e) => log::debug!("multiversion: untranslatable packet: {e}"),
                    }
                    while let Some(t) = a.pending() {
                        encoder.body(t, buf)?;
                    }
                    Ok(true)
                })
                .is_ok(),
                None => encoder.batch(&frames, threshold, buf).is_ok(),
            },
            Out::Compression(t) => {
                encoder.set_compression(t);
                true
            }
            Out::Encryption(secret) => {
                encoder.enable_encryption(&secret);
                true
            }
            Out::Adapter(a) => {
                *adapter = Some(a);
                true
            }
            Out::State(_) | Out::Player(_) => true,
            Out::Close => false,
        }
    }
}

/// Writes to a TCP socket.
pub(crate) async fn tcp_writer(
    mut socket: tokio::net::tcp::OwnedWriteHalf,
    mut rx: OutReceiver,
    queued: Arc<AtomicUsize>,
    compression_level: i32,
    sent: Arc<SentLog>,
    tap: Arc<PacketTap>,
    connection: u64,
) {
    let mut wire = Wire {
        encoder: FrameEncoder::new(),
        codec: Codec::new(compression_level),
        buf: Vec::with_capacity(8192),
        adapter: None,
    };
    let mut tap = OutTap::new(tap, connection);
    let mut open = true;
    // A full buffer was written with messages left in the queue.
    let mut more = false;
    // The buffers hold memory from a write since the last release.
    let mut holds = true;
    while open {
        let first = if more {
            rx.try_recv().ok()
        } else {
            match rx.next_write(holds).await {
                Next::Msg(msg) => Some(msg),
                Next::Gone => None,
                Next::Idle => {
                    wire.buf = Vec::new();
                    wire.encoder.trim(0);
                    holds = false;
                    continue;
                }
            }
        };
        holds = true;
        let Some(first) = first else {
            if more {
                more = false;
                continue;
            }
            break;
        };
        let ms = sent.now_ms();
        let mut next = Some(first);
        more = false;
        while let Some(msg) = next.take() {
            let consumed = msg.queued();
            if let Some(msg) = tap.filter(msg, &mut wire.codec) {
                record(&sent, ms, &msg);
                open = wire.write(msg);
            }
            queued.fetch_sub(consumed, Ordering::Relaxed);
            if !open {
                break;
            }
            if wire.buf.len() < 256 * 1024 {
                next = rx.try_recv().ok();
            } else {
                more = true;
            }
        }
        if !wire.buf.is_empty() {
            if socket.write_all(&wire.buf).await.is_err() {
                break;
            }
            wire.buf.clear();
        }
        // Bursts (the chunks of a join) grow the buffers; give that memory back.
        if wire.buf.capacity() > IDLE_BUFFER_MAX {
            wire.buf = Vec::with_capacity(8192);
        }
        wire.encoder.trim(IDLE_BUFFER_MAX);
    }
    let _ = socket.shutdown().await;
}

/// Forwards to a virtual connection: plain packet bodies, no framing, compression or cipher.
pub(crate) async fn virtual_writer(
    tx: mpsc::Sender<Vec<u8>>,
    mut rx: OutReceiver,
    queued: Arc<AtomicUsize>,
    sent: Arc<SentLog>,
    tap: Arc<PacketTap>,
    connection: u64,
) {
    let mut adapter: Option<Box<ClientboundHalf>> = None;
    let mut tap = OutTap::new(tap, connection);
    let mut codec = Codec::default();
    while let Some(msg) = rx.recv().await {
        queued.fetch_sub(msg.queued(), Ordering::Relaxed);
        let Some(msg) = tap.filter(msg, &mut codec) else {
            continue;
        };
        record(&sent, sent.now_ms(), &msg);
        let mut more = Vec::new();
        let body = match msg {
            Out::Batch(frames, threshold, _) => {
                let mut bodies = Vec::new();
                if for_each_body(&frames, threshold, |body| {
                    match adapter.as_mut() {
                        Some(a) => {
                            match a.translate(body) {
                                Ok(Some(t)) => bodies.push(t.to_vec()),
                                Ok(None) => {}
                                Err(e) => log::debug!("multiversion: untranslatable packet: {e}"),
                            }
                            while let Some(t) = a.pending() {
                                bodies.push(t.to_vec());
                            }
                        }
                        None => bodies.push(body.to_vec()),
                    }
                    Ok(true)
                })
                .is_err()
                {
                    break;
                }
                for body in bodies {
                    if tx.send(body).await.is_err() {
                        return;
                    }
                }
                continue;
            }
            Out::Body(b) | Out::Untapped(b) => match adapter.as_mut() {
                Some(a) => {
                    let t = a
                        .translate(&b)
                        .unwrap_or_else(|e| {
                            log::debug!("multiversion: untranslatable packet: {e}");
                            None
                        })
                        .map(<[u8]>::to_vec);
                    while let Some(t) = a.pending() {
                        more.push(t.to_vec());
                    }
                    t
                }
                None => Some(b),
            },
            Out::Shared(p) => match adapter.as_mut() {
                Some(a) => {
                    let t = a.translate_shared(&p).map(<[u8]>::to_vec);
                    while let Some(t) = a.pending() {
                        more.push(t.to_vec());
                    }
                    t
                }
                None => Some(p.body().to_vec()),
            },
            Out::Adapter(a) => {
                adapter = Some(a);
                continue;
            }
            Out::Compression(_) | Out::Encryption(_) | Out::State(_) | Out::Player(_) => continue,
            Out::Close => break,
        };
        if let Some(body) = body
            && tx.send(body).await.is_err()
        {
            break;
        }
        for body in more {
            if tx.send(body).await.is_err() {
                return;
            }
        }
    }
}

/// Notes a packet message in the connection's [`SentLog`] (before any version translation,
/// so the names are this version's).
fn record(sent: &SentLog, ms: u64, msg: &Out) {
    match msg {
        Out::Body(body) | Out::Untapped(body) => sent.body(ms, body),
        Out::Shared(p) => sent.shared(ms, p.id(), p.len()),
        Out::Batch(frames, threshold, _) => sent.frames(ms, frames, threshold.is_some()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vanillite_protocol::tap::{Interceptor, Mask};

    /// Listeners on packet ids 7 (rewritten per receiver), 8 (cancelled per receiver) and 9
    /// (rewritten per broadcast).
    struct Listeners;

    impl Interceptor for Listeners {
        fn broadcast(&self, _: State, id: i32, body: &[u8]) -> Verdict<Vec<u8>> {
            if id == 9 {
                Verdict::Replace([body, &[0xbb]].concat())
            } else {
                Verdict::Pass
            }
        }
        fn clientbound(&self, peer: Peer, state: State, id: i32, body: &[u8]) -> Verdict<Vec<u8>> {
            assert_eq!(state, State::Play);
            match id {
                7 => Verdict::Replace([body, &[u8::try_from(peer.player).unwrap_or(0)]].concat()),
                8 => Verdict::Drop,
                _ => Verdict::Pass,
            }
        }
        fn serverbound(&self, _: Peer, _: State, _: i32, _: &[u8]) -> Verdict<Vec<u8>> {
            Verdict::Pass
        }
    }

    fn tap(listen: bool) -> OutTap {
        let tap = Arc::new(PacketTap::new());
        tap.install(Arc::new(Listeners));
        if listen {
            let mut mask = Mask::default();
            mask.set(Stage::Receiver, State::Play, 7);
            mask.set(Stage::Receiver, State::Play, 8);
            mask.set(Stage::Broadcast, State::Play, 9);
            tap.store(&mask);
        }
        let mut out = OutTap::new(tap, 1);
        let mut codec = Codec::default();
        assert!(out.filter(Out::State(State::Play), &mut codec).is_none());
        assert!(out.filter(Out::Player(3), &mut codec).is_none());
        out
    }

    fn bodies(msg: Option<Out>) -> Vec<Vec<u8>> {
        let mut bodies = Vec::new();
        match msg {
            Some(Out::Body(body)) => bodies.push(body),
            Some(Out::Shared(packet)) => bodies.push(packet.body().to_vec()),
            Some(Out::Batch(frames, threshold, _)) => {
                let walked = for_each_body(&frames, threshold, |body| {
                    bodies.push(body.to_vec());
                    Ok(true)
                });
                assert!(walked.is_ok(), "{walked:?}");
            }
            Some(other) => panic!("unexpected {other:?}"),
            None => {}
        }
        bodies
    }

    fn batch(packets: &[Vec<u8>], threshold: Option<usize>) -> Vec<u8> {
        let mut codec = Codec::default();
        let mut frames = Vec::new();
        for body in packets {
            let framed = encode_frame(body, threshold, &mut codec, &mut frames);
            assert!(framed.is_ok(), "{framed:?}");
        }
        frames
    }

    #[test]
    fn without_listeners_messages_pass_untouched() {
        let mut tap = tap(false);
        let mut codec = Codec::default();
        let frames = batch(&[vec![7, 1], vec![8, 2]], Some(256));
        let Some(Out::Batch(same, Some(256), _)) = tap.filter(
            Out::Batch(frames.clone(), Some(256), BatchMark::UNKNOWN),
            &mut codec,
        ) else {
            panic!("the batch passes");
        };
        assert_eq!(same, frames);
        let shared = Arc::new(SharedPacket::new(vec![9, 1]));
        let Some(Out::Shared(same)) = tap.filter(Out::Shared(Arc::clone(&shared)), &mut codec)
        else {
            panic!("the shared packet passes");
        };
        assert!(Arc::ptr_eq(&same, &shared));
    }

    #[test]
    fn listeners_rewrite_and_cancel_single_and_shared_packets() {
        let mut tap = tap(true);
        let mut codec = Codec::default();
        assert_eq!(
            bodies(tap.filter(Out::Body(vec![7, 1]), &mut codec)),
            [[7, 1, 3]]
        );
        assert!(tap.filter(Out::Body(vec![8, 1]), &mut codec).is_none());
        assert_eq!(
            bodies(tap.filter(Out::Body(vec![5, 1]), &mut codec)),
            [[5, 1]]
        );
        // A body of its own passes the broadcast stage here.
        assert_eq!(
            bodies(tap.filter(Out::Body(vec![9, 1]), &mut codec)),
            [[9, 1, 0xbb]]
        );
        // Packets a plugin sends past the listeners.
        assert!(matches!(
            tap.filter(Out::Untapped(vec![8, 1]), &mut codec),
            Some(Out::Untapped(_))
        ));

        // A broadcast: every receiver gets the one replacement.
        let shared = Arc::new(SharedPacket::new(vec![9, 1]));
        let Some(Out::Shared(first)) = tap.filter(Out::Shared(Arc::clone(&shared)), &mut codec)
        else {
            panic!("still a shared packet");
        };
        let Some(Out::Shared(second)) = tap.filter(Out::Shared(Arc::clone(&shared)), &mut codec)
        else {
            panic!("still a shared packet");
        };
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.body(), [9, 1, 0xbb]);
        // Rewritten for this receiver: a copy of its own.
        let shared = Arc::new(SharedPacket::new(vec![7, 1]));
        assert!(matches!(
            tap.filter(Out::Shared(shared), &mut codec),
            Some(Out::Body(body)) if body == [7, 1, 3]
        ));
        let shared = Arc::new(SharedPacket::new(vec![8, 1]));
        assert!(tap.filter(Out::Shared(shared), &mut codec).is_none());
    }

    #[test]
    fn batches_are_framed_anew_only_around_listened_packets() {
        let mut tap = tap(true);
        let mut codec = Codec::default();
        let big = [vec![5], vec![0x55; 600]].concat();
        for threshold in [None, Some(256)] {
            // No packet with a listener: the very same frames, also with a compressed one
            // (inflated to look inside, but not framed anew).
            let plain = batch(&[vec![5, 1], vec![6, 2]], threshold);
            let Some(Out::Batch(same, ..)) = tap.filter(
                Out::Batch(plain.clone(), threshold, BatchMark::UNKNOWN),
                &mut codec,
            ) else {
                panic!("the batch passes");
            };
            assert_eq!(same, plain);

            let mixed = batch(
                &[vec![5, 1], vec![7, 2], big.clone(), vec![8, 3], vec![6, 4]],
                threshold,
            );
            assert_eq!(
                bodies(tap.filter(Out::Batch(mixed, threshold, BatchMark::UNKNOWN), &mut codec)),
                [vec![5, 1], vec![7, 2, 3], big.clone(), vec![6, 4]]
            );
            // Everything cancelled: nothing is written.
            let cancelled = batch(&[vec![8, 1], vec![8, 2]], threshold);
            assert!(
                tap.filter(
                    Out::Batch(cancelled, threshold, BatchMark::UNKNOWN),
                    &mut codec
                )
                .is_none()
            );
        }
    }
}
