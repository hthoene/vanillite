//! Bedrock network NBT (little-endian, VarInt lengths and ints): a compound writer for block
//! actor data and a skipper for the pre-encoded registries.

use crate::io::{self, Error, Reader, Result};

/// The entries of a network NBT compound, without the end tag.
#[derive(Debug, Default)]
pub struct Compound(Vec<u8>);

impl Compound {
    fn head(&mut self, kind: u8, name: &str) {
        self.0.push(kind);
        io::write_str(&mut self.0, name);
    }

    pub fn byte(&mut self, name: &str, v: u8) -> &mut Self {
        self.head(1, name);
        self.0.push(v);
        self
    }

    pub fn short(&mut self, name: &str, v: i16) -> &mut Self {
        self.head(2, name);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn int(&mut self, name: &str, v: i32) -> &mut Self {
        self.head(3, name);
        io::write_vari32(&mut self.0, v);
        self
    }

    pub fn string(&mut self, name: &str, v: &str) -> &mut Self {
        self.head(8, name);
        io::write_str(&mut self.0, v);
        self
    }

    pub fn compound(&mut self, name: &str, c: &Compound) -> &mut Self {
        self.head(10, name);
        self.0.extend_from_slice(&c.0);
        self.0.push(0);
        self
    }

    pub fn float(&mut self, name: &str, v: f32) -> &mut Self {
        self.head(5, name);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// A compound from its encoded payload (entries and end tag).
    pub fn raw_compound(&mut self, name: &str, payload: &[u8]) -> &mut Self {
        self.head(10, name);
        self.0.extend_from_slice(payload);
        self
    }

    pub fn strings(&mut self, name: &str, list: &[&str]) -> &mut Self {
        self.head(9, name);
        self.0.push(8);
        io::write_vari32(&mut self.0, i32::try_from(list.len()).unwrap_or(0));
        for s in list {
            io::write_str(&mut self.0, s);
        }
        self
    }

    pub fn ints(&mut self, name: &str, list: &[i32]) -> &mut Self {
        self.head(11, name);
        io::write_vari32(&mut self.0, i32::try_from(list.len()).unwrap_or(0));
        for &v in list {
            io::write_vari32(&mut self.0, v);
        }
        self
    }

    pub fn compounds(&mut self, name: &str, list: &[Compound]) -> &mut Self {
        self.head(9, name);
        self.0.push(10);
        io::write_vari32(&mut self.0, i32::try_from(list.len()).unwrap_or(0));
        for c in list {
            self.0.extend_from_slice(&c.0);
            self.0.push(0);
        }
        self
    }

    /// The compound as a nameless root tag.
    pub fn root(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len().saturating_add(3));
        out.extend_from_slice(&[10, 0]);
        out.extend_from_slice(&self.0);
        out.push(0);
        out
    }
}

/// Nesting limit of skipped tags.
const MAX_DEPTH: u8 = 32;

fn skip_payload(r: &mut Reader<'_>, kind: u8, depth: u8) -> Result<()> {
    let depth = depth.checked_sub(1).ok_or(Error::Invalid("nbt depth"))?;
    let len =
        |r: &mut Reader<'_>| usize::try_from(r.vari32()?).map_err(|_| Error::Invalid("nbt length"));
    match kind {
        1 => drop(r.u8()?),
        2 => drop(r.array::<2>()?),
        3 => drop(r.vari32()?),
        4 => drop(r.vari64()?),
        5 => drop(r.array::<4>()?),
        6 => drop(r.array::<8>()?),
        7 => {
            let n = len(r)?;
            r.take(n)?;
        }
        8 => drop(r.bytes("nbt string", 1 << 20)?),
        9 => {
            let elem = r.u8()?;
            let n = len(r)?;
            if n > r.remaining() {
                return Err(Error::Eof);
            }
            for _ in 0..n {
                skip_payload(r, elem, depth)?;
            }
        }
        10 => loop {
            let kind = r.u8()?;
            if kind == 0 {
                break;
            }
            r.bytes("nbt name", 1 << 16)?;
            skip_payload(r, kind, depth)?;
        },
        11 => {
            let n = len(r)?;
            for _ in 0..n.min(r.remaining()) {
                r.vari32()?;
            }
        }
        12 => {
            let n = len(r)?;
            for _ in 0..n.min(r.remaining()) {
                r.vari64()?;
            }
        }
        _ => return Err(Error::Invalid("nbt tag")),
    }
    Ok(())
}

/// Skips one named root tag.
pub fn skip_root(r: &mut Reader<'_>) -> Result<()> {
    let kind = r.u8()?;
    if kind == 0 {
        return Ok(());
    }
    r.bytes("nbt name", 1 << 16)?;
    skip_payload(r, kind, MAX_DEPTH)
}

/// Reads the `Text` of `FrontText` and `BackText` from the network NBT of a sign the client
/// edited (a named root compound).
pub fn sign_texts(r: &mut Reader<'_>) -> Result<[Option<String>; 2]> {
    let mut texts = [None, None];
    if r.u8()? != 10 {
        return Err(Error::Invalid("sign nbt"));
    }
    r.bytes("nbt name", 1 << 16)?;
    loop {
        let kind = r.u8()?;
        if kind == 0 {
            break;
        }
        let name = r.string("nbt name", 1 << 16)?;
        let side = match name {
            "FrontText" => Some(0usize),
            "BackText" => Some(1),
            _ => None,
        };
        match side {
            Some(side) if kind == 10 => loop {
                let kind = r.u8()?;
                if kind == 0 {
                    break;
                }
                let name = r.string("nbt name", 1 << 16)?;
                if kind == 8 && name == "Text" {
                    let text = r.string("sign text", 4096)?;
                    if let Some(t) = texts.get_mut(side) {
                        *t = Some(text.to_owned());
                    }
                } else {
                    skip_payload(r, kind, MAX_DEPTH)?;
                }
            },
            _ => skip_payload(r, kind, MAX_DEPTH)?,
        }
    }
    Ok(texts)
}
