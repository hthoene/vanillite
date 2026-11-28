//! Framing: the VarInt length prefix (at most 3 bytes, SEC-05), the optional compression layer
//! and the optional CFB8 cipher, for both directions.
//!
//! Inbound: [`FrameDecoder`] receives raw socket bytes, decrypts them as they arrive and yields
//! one uncompressed packet (`id` + body) at a time, rejecting frames above the per-state limit
//! before buffering them. Outbound: [`FrameEncoder`] frames, compresses and encrypts packets
//! into an output buffer; [`encode_frame`] builds shareable pre-encoded frames.

use crate::Packet;
use crate::cipher::Cfb8;
use crate::codec::{Error, Reader, Result, varint_len, write_varint};
use crate::compress::{self, Codec};

/// Largest frame the 3-byte length prefix can describe.
pub const MAX_FRAME: usize = 2_097_151;

/// Size limits for one connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest frame on the wire (after compression).
    pub max_frame: usize,
    /// Largest packet after decompression.
    pub max_uncompressed: usize,
}

impl Limits {
    /// Handshake: one intention packet (host name of 255 characters).
    pub const HANDSHAKE: Self = Self {
        max_frame: 1024,
        max_uncompressed: 1024,
    };
    /// Status: request and ping only.
    pub const STATUS: Self = Self {
        max_frame: 32,
        max_uncompressed: 32,
    };
    /// Login: hello, key, plugin answers (forwarding data), cookies.
    pub const LOGIN: Self = Self {
        max_frame: 32 * 1024,
        max_uncompressed: 32 * 1024,
    };
    /// Configuration: client information, payloads, known packs, cookies.
    pub const CONFIGURATION: Self = Self {
        max_frame: 128 * 1024,
        max_uncompressed: 128 * 1024,
    };
    /// Play: Vanilla's maxima.
    pub const PLAY: Self = Self {
        max_frame: MAX_FRAME,
        max_uncompressed: compress::MAX_UNCOMPRESSED,
    };
}

/// Inbound frame splitter.
#[derive(Debug)]
pub struct FrameDecoder {
    /// Received bytes in `pos..end`; the rest up to the length is spare space that stays
    /// initialized, so [`FrameDecoder::spare`] need not clear it again.
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    cipher: Option<Cfb8>,
    threshold: Option<usize>,
    limits: Limits,
    codec: Codec,
    inflated: Vec<u8>,
}

impl FrameDecoder {
    /// A decoder for a fresh connection (handshake limits, no cipher, no compression).
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(512),
            pos: 0,
            end: 0,
            cipher: None,
            threshold: None,
            limits: Limits::HANDSHAKE,
            codec: Codec::default(),
            inflated: Vec::new(),
        }
    }

    /// Sets the limits of the connection's current state.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Turns on decryption. Bytes already buffered but not yet consumed were received after the
    /// switch and are decrypted now.
    pub fn enable_encryption(&mut self, secret: &[u8; 16]) {
        let mut cipher = Cfb8::new(secret);
        if let Some(rest) = self.buf.get_mut(self.pos..self.end) {
            cipher.decrypt(rest);
        }
        self.cipher = Some(cipher);
    }

    /// Turns on compression with Vanilla's threshold semantics; a negative threshold turns it off.
    pub fn set_compression(&mut self, threshold: i32) {
        self.threshold = usize::try_from(threshold).ok();
    }

    /// Bytes received but not yet returned as frames.
    pub fn buffered(&self) -> usize {
        self.end.saturating_sub(self.pos)
    }

    /// Writable space of at least `n` bytes at the end of the buffer; call [`FrameDecoder::commit`]
    /// with the number of bytes written into it.
    pub fn spare(&mut self, n: usize) -> &mut [u8] {
        self.compact();
        let want = self.end.saturating_add(n);
        if self.buf.len() < want {
            self.buf.resize(want, 0);
        }
        self.buf.get_mut(self.end..want).unwrap_or_default()
    }

    /// Commits `n` bytes written into [`FrameDecoder::spare`] (which reserved `reserved`) and
    /// decrypts them.
    pub fn commit(&mut self, reserved: usize, n: usize) {
        let start = self.end;
        self.end = start.saturating_add(n.min(reserved)).min(self.buf.len());
        if let (Some(cipher), Some(new)) = (self.cipher.as_mut(), self.buf.get_mut(start..self.end))
        {
            cipher.decrypt(new);
        }
    }

    /// Copies received bytes in (a convenience for transports that own their buffers).
    pub fn push(&mut self, data: &[u8]) {
        let n = data.len();
        let spare = self.spare(n);
        spare.copy_from_slice(data);
        self.commit(n, n);
    }

    fn compact(&mut self) {
        if self.pos > 0 && (self.pos == self.end || self.pos > 4096) {
            self.buf.copy_within(self.pos..self.end, 0);
            self.end = self.end.saturating_sub(self.pos);
            self.pos = 0;
        }
    }

    /// The next complete packet (`id` + body, decompressed), or `None` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<&[u8]>> {
        match self.split()? {
            None => Ok(None),
            Some(Split::Plain { start, len }) => Ok(self.buf.get(start..start.saturating_add(len))),
            Some(Split::Compressed { body, data_len, .. }) => {
                let zlib = self.buf.get(body).unwrap_or_default();
                self.codec.decompress(zlib, data_len, &mut self.inflated)?;
                Ok(Some(&self.inflated))
            }
        }
    }

    /// The next complete frame without decompressing it, with the same checks as
    /// [`FrameDecoder::next_frame`]: a relay passes compressed frames on unchanged when both
    /// sides use the same threshold.
    pub fn next_raw(&mut self) -> Result<Option<RawFrame<'_>>> {
        Ok(match self.split()? {
            None => None,
            Some(Split::Plain { start, len }) => Some(RawFrame::Packet(
                self.buf
                    .get(start..start.saturating_add(len))
                    .unwrap_or_default(),
            )),
            Some(Split::Compressed {
                frame,
                body,
                data_len,
            }) => Some(RawFrame::Compressed {
                frame: self.buf.get(frame).unwrap_or_default(),
                zlib: self.buf.get(body).unwrap_or_default(),
                data_len,
            }),
        })
    }

    /// Takes the next complete frame off the buffer and checks its limits.
    fn split(&mut self) -> Result<Option<Split>> {
        let avail = self.buf.get(self.pos..self.end).unwrap_or_default();
        // Frame length: VarInt of at most 3 bytes.
        let mut len: usize = 0;
        let mut header = 0usize;
        let mut complete = false;
        for (i, b) in avail.iter().take(3).enumerate() {
            len |=
                usize::from(b & 0x7F).wrapping_shl(u32::try_from(i).unwrap_or(0).wrapping_mul(7));
            header = i.saturating_add(1);
            if b & 0x80 == 0 {
                complete = true;
                break;
            }
        }
        if !complete {
            return if avail.len() >= 3 {
                Err(Error::BadFrame("length prefix longer than 3 bytes"))
            } else {
                Ok(None)
            };
        }
        if len == 0 {
            return Err(Error::BadFrame("empty frame"));
        }
        if len > self.limits.max_frame {
            return Err(Error::TooLong {
                what: "frame",
                len,
                max: self.limits.max_frame,
            });
        }
        let end = header.saturating_add(len);
        let Some(frame) = avail.get(header..end) else {
            return Ok(None);
        };
        let frame_start = self.pos.saturating_add(header);
        self.pos = self.pos.saturating_add(end);
        let Some(threshold) = self.threshold else {
            return Ok(Some(Split::Plain {
                start: frame_start,
                len,
            }));
        };
        let mut r = crate::codec::Reader::new(frame);
        let data_len = usize::try_from(r.varint()?).map_err(|_| Error::NegativeLength)?;
        let body = r.rest();
        let start = frame_start.saturating_add(frame.len().saturating_sub(body.len()));
        let body_len = body.len();
        if data_len == 0 {
            if body_len > self.limits.max_uncompressed {
                return Err(Error::TooLong {
                    what: "packet",
                    len: body_len,
                    max: self.limits.max_uncompressed,
                });
            }
            return Ok(Some(Split::Plain {
                start,
                len: body_len,
            }));
        }
        if data_len < threshold {
            return Err(Error::BadFrame("compressed packet below threshold"));
        }
        if data_len > self.limits.max_uncompressed {
            return Err(Error::TooLong {
                what: "packet",
                len: data_len,
                max: self.limits.max_uncompressed,
            });
        }
        Ok(Some(Split::Compressed {
            frame: frame_start..frame_start.saturating_add(len),
            body: start..start.saturating_add(body_len),
            data_len,
        }))
    }
}

/// Where [`FrameDecoder::split`] found a frame in the buffer.
enum Split {
    Plain {
        start: usize,
        len: usize,
    },
    Compressed {
        frame: std::ops::Range<usize>,
        body: std::ops::Range<usize>,
        data_len: usize,
    },
}

/// A frame from [`FrameDecoder::next_raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFrame<'a> {
    /// An uncompressed packet (`id` + body).
    Packet(&'a [u8]),
    /// A compressed packet as it was received.
    Compressed {
        /// The frame after its length prefix (data length, then the zlib stream).
        frame: &'a [u8],
        /// The zlib stream.
        zlib: &'a [u8],
        /// The length after decompression (checked against threshold and limits).
        data_len: usize,
    },
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Outbound framer.
#[derive(Debug, Default)]
pub struct FrameEncoder {
    cipher: Option<Cfb8>,
    threshold: Option<usize>,
    codec: Codec,
    scratch: Vec<u8>,
    compressed: Vec<u8>,
}

impl FrameEncoder {
    /// An encoder without cipher and compression.
    pub fn new() -> Self {
        Self::default()
    }

    /// Turns on encryption for everything appended from now on.
    pub fn enable_encryption(&mut self, secret: &[u8; 16]) {
        self.cipher = Some(Cfb8::new(secret));
    }

    /// Sets the compression threshold (negative = off).
    pub fn set_compression(&mut self, threshold: i32) {
        self.threshold = usize::try_from(threshold).ok();
    }

    /// Sets the libdeflate level of outbound compression (1..=12).
    pub fn set_compression_level(&mut self, level: i32) {
        self.codec = Codec::new(level);
    }

    /// The compression threshold in use.
    pub fn threshold(&self) -> Option<usize> {
        self.threshold
    }

    /// Releases scratch buffers that grew beyond `max` bytes (after a burst such as the
    /// chunks of a join), so an idle connection keeps little memory.
    pub fn trim(&mut self, max: usize) {
        for buffer in [&mut self.scratch, &mut self.compressed] {
            if buffer.capacity() > max {
                *buffer = Vec::new();
            }
        }
    }

    /// Frames and appends a typed packet.
    pub fn packet<P: Packet>(&mut self, packet: &P, out: &mut Vec<u8>) -> Result<()> {
        let mut body = std::mem::take(&mut self.scratch);
        body.clear();
        write_varint(&mut body, P::ID);
        packet.encode(&mut body);
        let r = self.body(&body, out);
        self.scratch = body;
        r
    }

    /// Frames and appends an encoded packet (`id` + body).
    pub fn body(&mut self, body: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let start = out.len();
        encode_frame_with(
            body,
            self.threshold,
            &mut self.codec,
            &mut self.compressed,
            out,
        )?;
        self.encrypt_from(out, start);
        Ok(())
    }

    /// Appends a frame that was pre-encoded with [`encode_frame`] for this connection's
    /// compression setting.
    pub fn frame(&mut self, frame: &[u8], out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(frame);
        self.encrypt_from(out, start);
    }

    /// Appends a frame received with [`FrameDecoder::next_raw`] (without its length prefix)
    /// from a connection with the same compression threshold.
    pub fn raw(&mut self, frame: &[u8], out: &mut Vec<u8>) -> Result<()> {
        check_frame(frame.len())?;
        let start = out.len();
        write_varint(
            out,
            i32::try_from(frame.len()).map_err(|_| Error::BadFrame("frame too long"))?,
        );
        out.extend_from_slice(frame);
        self.encrypt_from(out, start);
        Ok(())
    }

    /// Appends and encrypts a batch of frames built with [`encode_frame`] for `threshold`.
    /// A batch for this connection's threshold is copied as a whole; any other is re-framed
    /// packet by packet.
    pub fn batch(
        &mut self,
        frames: &[u8],
        threshold: Option<usize>,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        if threshold == self.threshold {
            self.frame(frames, out);
            return Ok(());
        }
        for_each_body(frames, threshold, |body| {
            self.body(body, out).map(|()| true)
        })
    }

    fn encrypt_from(&mut self, out: &mut [u8], start: usize) {
        if let (Some(cipher), Some(tail)) = (self.cipher.as_mut(), out.get_mut(start..)) {
            cipher.encrypt(tail);
        }
    }
}

/// Calls `f` with every packet body of `frames`, frames built with [`encode_frame`] for
/// `threshold`, until `f` returns false.
pub fn for_each_body(
    frames: &[u8],
    threshold: Option<usize>,
    mut f: impl FnMut(&[u8]) -> Result<bool>,
) -> Result<()> {
    let mut decoder = FrameDecoder::new();
    decoder.set_limits(Limits::PLAY);
    decoder.set_compression(threshold.and_then(|t| i32::try_from(t).ok()).unwrap_or(-1));
    decoder.push(frames);
    while let Some(body) = decoder.next_frame()? {
        if !f(body)? {
            break;
        }
    }
    Ok(())
}

/// Calls `f` with the packet id of every frame of `frames` (built with [`encode_frame`] for
/// `threshold`) until `f` returns false, without copying or inflating: a compressed frame
/// reports `None`.
pub fn for_each_id(
    frames: &[u8],
    threshold: Option<usize>,
    mut f: impl FnMut(Option<i32>) -> bool,
) -> Result<()> {
    let mut frames = Reader::new(frames);
    while frames.remaining() > 0 {
        let len = usize::try_from(frames.varint()?).map_err(|_| Error::BadFrame("frame length"))?;
        let mut frame = Reader::new(frames.take(len)?);
        let id = if threshold.is_some() && frame.varint()? != 0 {
            None
        } else {
            Some(frame.varint()?)
        };
        if !f(id) {
            break;
        }
    }
    Ok(())
}

/// Builds a frame (length prefix, compression layer if `threshold` is set) for `body`.
pub fn encode_frame(
    body: &[u8],
    threshold: Option<usize>,
    codec: &mut Codec,
    out: &mut Vec<u8>,
) -> Result<()> {
    encode_frame_with(body, threshold, codec, &mut Vec::new(), out)
}

/// [`encode_frame`] with a reusable buffer for the compressed body.
fn encode_frame_with(
    body: &[u8],
    threshold: Option<usize>,
    codec: &mut Codec,
    compressed: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<()> {
    if body.len() > compress::MAX_UNCOMPRESSED {
        return Err(Error::TooLong {
            what: "packet",
            len: body.len(),
            max: compress::MAX_UNCOMPRESSED,
        });
    }
    let len_i32 = |n: usize| i32::try_from(n).map_err(|_| Error::BadFrame("frame too long"));
    match threshold {
        None => {
            check_frame(body.len())?;
            write_varint(out, len_i32(body.len())?);
            out.extend_from_slice(body);
        }
        Some(t) if body.len() < t => {
            check_frame(body.len().saturating_add(1))?;
            write_varint(out, len_i32(body.len().saturating_add(1))?);
            out.push(0);
            out.extend_from_slice(body);
        }
        Some(_) => {
            compressed.clear();
            codec.compress(body, compressed)?;
            let data_len = len_i32(body.len())?;
            let frame_len = varint_len(data_len).saturating_add(compressed.len());
            check_frame(frame_len)?;
            write_varint(out, len_i32(frame_len)?);
            write_varint(out, data_len);
            out.extend_from_slice(compressed);
        }
    }
    Ok(())
}

fn check_frame(len: usize) -> Result<()> {
    if len > MAX_FRAME {
        Err(Error::TooLong {
            what: "frame",
            len,
            max: MAX_FRAME,
        })
    } else {
        Ok(())
    }
}

/// Encodes a typed packet as `id` + body.
pub fn encode_body<P: Packet>(packet: &P) -> Vec<u8> {
    let mut body = Vec::new();
    write_varint(&mut body, P::ID);
    packet.encode(&mut body);
    body
}
