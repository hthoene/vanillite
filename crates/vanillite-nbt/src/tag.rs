/// Binary type id of a tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TagType {
    End = 0,
    Byte = 1,
    Short = 2,
    Int = 3,
    Long = 4,
    Float = 5,
    Double = 6,
    ByteArray = 7,
    String = 8,
    List = 9,
    Compound = 10,
    IntArray = 11,
    LongArray = 12,
}

impl TagType {
    /// The type for a binary id.
    pub const fn from_id(id: u8) -> Option<Self> {
        Some(match id {
            0 => Self::End,
            1 => Self::Byte,
            2 => Self::Short,
            3 => Self::Int,
            4 => Self::Long,
            5 => Self::Float,
            6 => Self::Double,
            7 => Self::ByteArray,
            8 => Self::String,
            9 => Self::List,
            10 => Self::Compound,
            11 => Self::IntArray,
            12 => Self::LongArray,
            _ => return None,
        })
    }

    /// The binary id.
    #[allow(clippy::as_conversions)] // Fieldless `repr(u8)` enum to its discriminant.
    pub const fn id(self) -> u8 {
        self as u8
    }
}

/// An owned NBT value.
#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<i8>),
    String(String),
    List(List),
    Compound(Compound),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Tag {
    /// Turns every `-0.0` float and double inside into `0.0`, as Vanilla's tags never hold
    /// one (`FloatTag.valueOf` and `DoubleTag.valueOf` return their cached zero for it).
    pub fn positive_zeros(&mut self) {
        match self {
            Tag::Float(v) if *v == 0.0 => *v = 0.0,
            Tag::Double(v) if *v == 0.0 => *v = 0.0,
            Tag::List(l) => l.0.iter_mut().for_each(Tag::positive_zeros),
            Tag::Compound(c) => c.positive_zeros(),
            _ => {}
        }
    }

    /// The binary type of this value.
    pub const fn tag_type(&self) -> TagType {
        match self {
            Self::Byte(_) => TagType::Byte,
            Self::Short(_) => TagType::Short,
            Self::Int(_) => TagType::Int,
            Self::Long(_) => TagType::Long,
            Self::Float(_) => TagType::Float,
            Self::Double(_) => TagType::Double,
            Self::ByteArray(_) => TagType::ByteArray,
            Self::String(_) => TagType::String,
            Self::List(_) => TagType::List,
            Self::Compound(_) => TagType::Compound,
            Self::IntArray(_) => TagType::IntArray,
            Self::LongArray(_) => TagType::LongArray,
        }
    }

    /// The string value, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// The compound value, if this is a compound.
    pub fn as_compound(&self) -> Option<&Compound> {
        match self {
            Self::Compound(c) => Some(c),
            _ => None,
        }
    }

    /// Any numeric value widened to `i64` (floating values are truncated like Java's casts).
    #[allow(clippy::as_conversions)]
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Byte(v) => Some(i64::from(v)),
            Self::Short(v) => Some(i64::from(v)),
            Self::Int(v) => Some(i64::from(v)),
            Self::Long(v) => Some(v),
            // `as` saturates and maps NaN to 0, which is what Java's (long) cast does.
            Self::Float(v) => Some(v as i64),
            Self::Double(v) => Some(v as i64),
            _ => None,
        }
    }
}

macro_rules! from_impl {
    ($($t:ty => $v:ident),* $(,)?) => {
        $(impl From<$t> for Tag {
            fn from(v: $t) -> Self {
                Self::$v(v.into())
            }
        })*
    };
}

from_impl! {
    i8 => Byte, i16 => Short, i32 => Int, i64 => Long, f32 => Float, f64 => Double,
    String => String, &str => String, List => List, Compound => Compound,
    Vec<i8> => ByteArray, Vec<i32> => IntArray, Vec<i64> => LongArray,
}

impl From<bool> for Tag {
    fn from(v: bool) -> Self {
        Self::Byte(i8::from(v))
    }
}

/// A list of values. Lists of one element type are written as such; lists with mixed types are
/// written like Vanilla does since 1.21.5: as a compound list where each non-compound element (and
/// each compound that would read back ambiguously) is wrapped as `{"": value}`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct List(pub Vec<Tag>);

impl List {
    /// An empty list.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// The element type the list is written with.
    pub fn element_type(&self) -> TagType {
        let mut iter = self.0.iter();
        let Some(first) = iter.next() else {
            return TagType::End;
        };
        let t = first.tag_type();
        if iter.all(|e| e.tag_type() == t) {
            t
        } else {
            TagType::Compound
        }
    }

    /// Whether elements are written wrapped (mixed types).
    pub fn is_heterogeneous(&self) -> bool {
        self.0
            .first()
            .is_some_and(|f| self.0.iter().any(|e| e.tag_type() != f.tag_type()))
    }
}

/// A compound: named values in insertion order. Lookups take the last entry of a name, so
/// duplicate names in input behave like Java's map (last one wins) without quadratic checks while
/// reading.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Compound(Vec<(String, Tag)>);

impl Compound {
    /// An empty compound.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// An empty compound with room for `n` entries.
    pub fn with_capacity(n: usize) -> Self {
        Self(Vec::with_capacity(n))
    }

    /// Sets `name` to `value`, replacing an existing entry of that name.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<Tag>) {
        let name = name.into();
        let value = value.into();
        match self.0.iter_mut().rev().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name, value)),
        }
    }

    /// Builder form of [`Compound::insert`].
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<Tag>) -> Self {
        self.insert(name, value);
        self
    }

    /// Appends without checking for an existing entry (used by readers).
    pub(crate) fn push_unchecked(&mut self, name: String, value: Tag) {
        self.0.push((name, value));
    }

    /// The value of `name`.
    pub fn get(&self, name: &str) -> Option<&Tag> {
        self.0.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// The value of `name`, mutably.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Tag> {
        self.0
            .iter_mut()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    /// Removes and returns the value of `name`.
    pub fn remove(&mut self, name: &str) -> Option<Tag> {
        let idx = self.0.iter().rposition(|(n, _)| n == name)?;
        Some(self.0.remove(idx).1)
    }

    /// [`Tag::positive_zeros`] of every value.
    pub fn positive_zeros(&mut self) {
        self.0.iter_mut().for_each(|(_, v)| v.positive_zeros());
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Tag)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v))
    }

    /// The wrapped value if this compound is the `{"": value}` wrapper of a heterogeneous list
    /// element, otherwise the compound itself.
    pub(crate) fn into_list_wrapper(mut self) -> std::result::Result<Tag, Self> {
        if self.as_list_wrapper().is_none() {
            return Err(self);
        }
        match self.0.pop() {
            Some((_, value)) => Ok(value),
            None => Err(self),
        }
    }

    /// Whether this compound is the `{"": value}` wrapper of a heterogeneous list element.
    pub(crate) fn as_list_wrapper(&self) -> Option<&Tag> {
        match self.0.as_slice() {
            [(name, value)] if name.is_empty() => Some(value),
            _ => None,
        }
    }

    /// Reorders the entries into the iteration order of a `java.util.HashMap` that got them in
    /// the current order. Vanilla's `CompoundTag` is such a map, so codec output written by
    /// Vanilla has this order on the wire.
    pub fn sort_java_hash_order(&mut self) {
        // Default capacity 16, doubled whenever the size exceeds 3/4 of it.
        let mut capacity: u32 = 16;
        while self.0.len()
            > usize::try_from((capacity >> 2).saturating_mul(3)).unwrap_or(usize::MAX)
        {
            capacity = capacity.saturating_mul(2);
        }
        let mask = capacity.wrapping_sub(1);
        // A stable sort keeps the insertion order inside a bucket, like the bucket lists.
        self.0
            .sort_by_key(|(name, _)| java_spread_hash(name) & mask);
    }

    /// Builder form of [`Compound::sort_java_hash_order`].
    #[must_use]
    pub fn in_java_hash_order(mut self) -> Self {
        self.sort_java_hash_order();
        self
    }
}

/// `HashMap.hash(key)` of a `String`: `hashCode()` over UTF-16 code units, spread by
/// `h ^ (h >>> 16)`.
fn java_spread_hash(s: &str) -> u32 {
    let mut buf = [0u16; 2];
    let h = s.chars().fold(0u32, |h, c| {
        c.encode_utf16(&mut buf).iter().fold(h, |h, &unit| {
            h.wrapping_mul(31).wrapping_add(u32::from(unit))
        })
    });
    h ^ (h >> 16)
}
