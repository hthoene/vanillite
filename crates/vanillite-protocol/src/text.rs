//! Text components: Vanilla's full component model with all contents, styles, click and hover
//! events, and their NBT and JSON forms (`ComponentSerialization`).
//!
//! Components go on the wire as NBT in configuration and play, and as JSON in status and login
//! disconnects. The NBT form is byte-exact with Vanilla: compounds are written in the iteration
//! order of Java's `HashMap` (Vanilla's `CompoundTag`), the JSON form in codec field order
//! (Gson). Decoding is depth-limited (SEC-04). Text that comes from players is only ever wrapped
//! as a literal (SEC-09); nothing here interprets player input as rich text.

use std::fmt;

use crate::codec::Encode;
use serde_json::{Map, Number, Value};
use vanillite_nbt::{Compound, List, Tag};

/// Maximum nesting of components (siblings, arguments, hover texts, separators) when decoding
/// (SEC-04). Every component level takes two NBT levels, so Vanilla's NBT depth limit of 512
/// allows at most 256.
pub const MAX_DEPTH: usize = 256;

/// Why a component could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextError(pub std::borrow::Cow<'static, str>);

impl TextError {
    /// An error with a fixed reason.
    pub const fn new(reason: &'static str) -> Self {
        Self(std::borrow::Cow::Borrowed(reason))
    }
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid text component: {}", self.0)
    }
}

impl std::error::Error for TextError {}

type Result<T> = std::result::Result<T, TextError>;

/// The sixteen named chat colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedColor {
    Black,
    DarkBlue,
    DarkGreen,
    DarkAqua,
    DarkRed,
    DarkPurple,
    Gold,
    Gray,
    DarkGray,
    Blue,
    Green,
    Aqua,
    Red,
    LightPurple,
    Yellow,
    White,
}

impl NamedColor {
    /// All named colours.
    pub const ALL: [NamedColor; 16] = [
        Self::Black,
        Self::DarkBlue,
        Self::DarkGreen,
        Self::DarkAqua,
        Self::DarkRed,
        Self::DarkPurple,
        Self::Gold,
        Self::Gray,
        Self::DarkGray,
        Self::Blue,
        Self::Green,
        Self::Aqua,
        Self::Red,
        Self::LightPurple,
        Self::Yellow,
        Self::White,
    ];

    /// The Vanilla name (`dark_blue`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::DarkBlue => "dark_blue",
            Self::DarkGreen => "dark_green",
            Self::DarkAqua => "dark_aqua",
            Self::DarkRed => "dark_red",
            Self::DarkPurple => "dark_purple",
            Self::Gold => "gold",
            Self::Gray => "gray",
            Self::DarkGray => "dark_gray",
            Self::Blue => "blue",
            Self::Green => "green",
            Self::Aqua => "aqua",
            Self::Red => "red",
            Self::LightPurple => "light_purple",
            Self::Yellow => "yellow",
            Self::White => "white",
        }
    }

    /// The colour of a Vanilla name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// A text colour (`TextColor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    Named(NamedColor),
    /// `0xRRGGBB`.
    Rgb(u32),
}

impl Color {
    /// `TextColor.serialize`: the name, or `#RRGGBB`.
    pub fn serialize(self) -> String {
        match self {
            Self::Named(n) => n.name().to_owned(),
            Self::Rgb(v) => format!("#{:06X}", v & 0xFF_FFFF),
        }
    }

    /// `TextColor.parseColor`: a name or `#` with a hexadecimal value up to `0xFFFFFF`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.strip_prefix('#') {
            Some(hex) => u32::from_str_radix(hex, 16)
                .ok()
                .filter(|v| *v <= 0xFF_FFFF)
                .map(Self::Rgb),
            None => NamedColor::from_name(s).map(Self::Named),
        }
    }
}

/// An NBT value the component model carries without interpreting it (dialogs, item
/// components, profiles, custom payloads). Equality compares floats bit by bit.
#[derive(Debug, Clone)]
pub struct OpaqueNbt(pub Tag);

impl PartialEq for OpaqueNbt {
    fn eq(&self, other: &Self) -> bool {
        tag_eq(&self.0, &other.0)
    }
}

impl Eq for OpaqueNbt {}

fn tag_eq(a: &Tag, b: &Tag) -> bool {
    match (a, b) {
        (Tag::Float(x), Tag::Float(y)) => x.to_bits() == y.to_bits(),
        (Tag::Double(x), Tag::Double(y)) => x.to_bits() == y.to_bits(),
        (Tag::List(x), Tag::List(y)) => {
            x.0.len() == y.0.len() && x.0.iter().zip(&y.0).all(|(a, b)| tag_eq(a, b))
        }
        (Tag::Compound(x), Tag::Compound(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((ka, va), (kb, vb))| ka == kb && tag_eq(va, vb))
        }
        _ => a == b,
    }
}

/// What a click on the text does (`ClickEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickEvent {
    /// Opens an `http`/`https` URL.
    OpenUrl(String),
    /// Opens a local file (client-side only; never sent by servers).
    OpenFile(String),
    /// Runs a command or sends a chat message.
    RunCommand(String),
    /// Puts text into the chat input.
    SuggestCommand(String),
    /// Shows a dialog: its registry id (string) or an inline dialog.
    ShowDialog(OpaqueNbt),
    /// Turns a book page.
    ChangePage(i32),
    /// Copies text to the clipboard.
    CopyToClipboard(String),
    /// Sends a custom action back to the server.
    Custom {
        id: String,
        payload: Option<OpaqueNbt>,
    },
}

impl ClickEvent {
    /// The action name.
    pub const fn action(&self) -> &'static str {
        match self {
            Self::OpenUrl(_) => "open_url",
            Self::OpenFile(_) => "open_file",
            Self::RunCommand(_) => "run_command",
            Self::SuggestCommand(_) => "suggest_command",
            Self::ShowDialog(_) => "show_dialog",
            Self::ChangePage(_) => "change_page",
            Self::CopyToClipboard(_) => "copy_to_clipboard",
            Self::Custom { .. } => "custom",
        }
    }
}

/// What hovering over the text shows (`HoverEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverEvent {
    /// A text.
    ShowText(Component),
    /// An item tooltip.
    ShowItem {
        /// Item id.
        id: String,
        /// Count (1..=99).
        count: i32,
        /// Data component patch.
        components: Option<OpaqueNbt>,
    },
    /// An entity tooltip.
    ShowEntity {
        /// Entity type id.
        id: String,
        /// Entity UUID.
        uuid: u128,
        /// Display name.
        name: Option<Component>,
    },
}

impl HoverEvent {
    /// The action name.
    pub const fn action(&self) -> &'static str {
        match self {
            Self::ShowText(_) => "show_text",
            Self::ShowItem { .. } => "show_item",
            Self::ShowEntity { .. } => "show_entity",
        }
    }
}

/// Style attributes; `None` inherits from the parent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Style {
    pub color: Option<Color>,
    /// ARGB shadow colour.
    pub shadow_color: Option<i32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underlined: Option<bool>,
    pub strikethrough: Option<bool>,
    pub obfuscated: Option<bool>,
    pub click_event: Option<ClickEvent>,
    pub hover_event: Option<Box<HoverEvent>>,
    pub insertion: Option<String>,
    /// Font id.
    pub font: Option<String>,
}

impl Style {
    /// Whether no attribute is set (`Style.isEmpty`).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The NBT form (`Style.Serializer.CODEC`): a compound with the style fields.
    pub fn to_nbt(&self) -> Tag {
        let mut c = Compound::new();
        style_to_nbt(self, &mut c);
        Tag::Compound(c.in_java_hash_order())
    }

    /// Decodes a style from NBT (`Style.Serializer.CODEC`): a compound with the style fields.
    pub fn try_from_nbt(tag: &Tag) -> Result<Self> {
        let v = NbtIn(tag);
        if !v.is_map() {
            return Err(TextError::new("not a map"));
        }
        let mut style = decode_style(&v)?;
        if let Some(e) = v.field("hover_event") {
            style.hover_event = Some(decode_hover(&e, 1)?);
        }
        Ok(style)
    }
}

/// Fills `%s`, `%<n>$s` and `%%` of a translation template (`decomposeTemplate`); `false` on
/// another conversion or a missing argument.
/// A translation argument that is not a component, as `getString` shows it.
fn push_scalar_arg(arg: &Arg, out: &mut String) {
    match arg {
        Arg::Component(_) => {}
        Arg::String(v) => out.push_str(v),
        Arg::Byte(v) => out.push_str(&v.to_string()),
        Arg::Short(v) => out.push_str(&v.to_string()),
        Arg::Int(v) => out.push_str(&v.to_string()),
        Arg::Long(v) => out.push_str(&v.to_string()),
        Arg::Float(v) => out.push_str(&java_float(*v)),
        Arg::Double(v) => out.push_str(&java_double(*v)),
        Arg::Bool(v) => out.push_str(&v.to_string()),
    }
}

/// `Double.toString` / `Float.toString`: plain decimals in `[1e-3, 1e7)`, otherwise Java's
/// scientific form (`1.0E7`).
fn java_decimal(abs: f64, plain: String, scientific: &str) -> String {
    if plain == "NaN" {
        return plain;
    }
    if plain.ends_with("inf") {
        return plain.replace("inf", "Infinity");
    }
    if abs == 0.0 || (1.0e-3..1.0e7).contains(&abs) {
        return plain;
    }
    match scientific.split_once('e') {
        Some((mantissa, exponent)) if mantissa.contains('.') => format!("{mantissa}E{exponent}"),
        Some((mantissa, exponent)) => format!("{mantissa}.0E{exponent}"),
        None => plain,
    }
}

/// `Double.toString`.
pub fn java_double(v: f64) -> String {
    java_decimal(v.abs(), format!("{v:?}"), &format!("{v:e}"))
}

/// `Float.toString`.
pub fn java_float(v: f32) -> String {
    java_decimal(f64::from(v).abs(), format!("{v:?}"), &format!("{v:e}"))
}

/// The resolved colour and formats of a part, for [`Component::to_legacy_in`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Legacy {
    color: Option<Color>,
    bold: bool,
    italic: bool,
    underlined: bool,
    strikethrough: bool,
    obfuscated: bool,
}

impl Legacy {
    /// The style of a child: what it sets, else the parent's.
    fn inherit(self, style: &Style) -> Self {
        Self {
            color: style.color.or(self.color),
            bold: style.bold.unwrap_or(self.bold),
            italic: style.italic.unwrap_or(self.italic),
            underlined: style.underlined.unwrap_or(self.underlined),
            strikethrough: style.strikethrough.unwrap_or(self.strikethrough),
            obfuscated: style.obfuscated.unwrap_or(self.obfuscated),
        }
    }

    /// Writes the codes that switch from `last` to this style (nothing if it is the same).
    fn switch(self, last: &mut Legacy, out: &mut String) {
        if *last == self {
            return;
        }
        *last = self;
        out.push_str("§r");
        match self.color {
            Some(Color::Named(named)) => {
                let index = NamedColor::ALL.iter().position(|c| *c == named);
                if let Some(code) = index
                    .and_then(|i| u32::try_from(i).ok())
                    .and_then(|i| char::from_digit(i, 16))
                {
                    out.push('§');
                    out.push(code);
                }
            }
            Some(Color::Rgb(rgb)) => {
                out.push_str("§x");
                for digit in format!("{:06x}", rgb & 0xff_ffff).chars() {
                    out.push('§');
                    out.push(digit);
                }
            }
            None => {}
        }
        for (on, code) in [
            (self.obfuscated, "§k"),
            (self.bold, "§l"),
            (self.strikethrough, "§m"),
            (self.underlined, "§n"),
            (self.italic, "§o"),
        ] {
            if on {
                out.push_str(code);
            }
        }
    }
}

fn fill_template(
    format: &str,
    out: &mut String,
    mut arg: impl FnMut(usize, &mut String) -> bool,
) -> bool {
    let mut next = 0usize;
    let mut rest = format;
    while let Some(at) = rest.find('%') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let spec = rest.get(at.saturating_add(1)..).unwrap_or_default();
        let digits = spec.bytes().take_while(u8::is_ascii_digit).count();
        let (index, after) = match spec.get(digits..).and_then(|s| s.strip_prefix('$')) {
            Some(after) if digits > 0 => (
                spec.get(..digits)
                    .and_then(|d| d.parse::<usize>().ok())
                    .and_then(|n| n.checked_sub(1)),
                after,
            ),
            _ => (None, spec),
        };
        let mut chars = after.chars();
        match chars.next() {
            Some('%') if digits == 0 => out.push('%'),
            Some('s') => {
                let i = match index {
                    Some(i) => i,
                    None if digits > 0 => return false,
                    None => {
                        let i = next;
                        next = next.saturating_add(1);
                        i
                    }
                };
                if !arg(i, out) {
                    return false;
                }
            }
            Some(_) | None => return false,
        }
        rest = chars.as_str();
    }
    out.push_str(rest);
    true
}

/// An argument of a translatable component: a component or a primitive (`Number`, `Boolean`,
/// `String`). Equality compares floats bit by bit.
#[derive(Debug, Clone)]
pub enum Arg {
    Component(Box<Component>),
    String(String),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
}

impl PartialEq for Arg {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Component(a), Self::Component(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Byte(a), Self::Byte(b)) => a == b,
            (Self::Short(a), Self::Short(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Long(a), Self::Long(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            (Self::Double(a), Self::Double(b)) => a.to_bits() == b.to_bits(),
            (Self::Bool(a), Self::Bool(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Arg {}

impl From<Component> for Arg {
    fn from(c: Component) -> Self {
        Self::Component(Box::new(c))
    }
}

impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

impl From<String> for Arg {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<i32> for Arg {
    fn from(v: i32) -> Self {
        Self::Int(v)
    }
}

impl From<i64> for Arg {
    fn from(v: i64) -> Self {
        Self::Long(v)
    }
}

impl From<f32> for Arg {
    fn from(v: f32) -> Self {
        Self::Float(v)
    }
}

impl From<f64> for Arg {
    fn from(v: f64) -> Self {
        Self::Double(v)
    }
}

impl From<bool> for Arg {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

/// Where an `nbt` component reads its data from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NbtSource {
    /// An entity selector.
    Entity(String),
    /// Block coordinates.
    Block(String),
    /// A command storage id.
    Storage(String),
}

/// What an `object` component shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectInfo {
    /// A sprite of an atlas (`atlas` defaults to `minecraft:blocks`).
    Atlas { atlas: String, sprite: String },
    /// A player head; `player` is the profile (name or partial profile).
    Player { player: OpaqueNbt, hat: bool },
}

/// The default atlas of atlas objects.
const DEFAULT_ATLAS: &str = "minecraft:blocks";

/// What a component shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// Literal text.
    Text(String),
    /// A translation key with arguments, resolved by the client.
    Translatable {
        key: String,
        fallback: Option<String>,
        args: Vec<Arg>,
    },
    /// A scoreboard score of a name or selector.
    Score { name: String, objective: String },
    /// The names of the entities an entity selector finds.
    Selector {
        pattern: String,
        separator: Option<Box<Component>>,
    },
    /// A key binding, resolved by the client.
    Keybind(String),
    /// NBT data of an entity, block or storage.
    Nbt {
        path: String,
        interpret: bool,
        plain: bool,
        separator: Option<Box<Component>>,
        source: NbtSource,
    },
    /// A sprite or player head.
    Object {
        info: ObjectInfo,
        fallback: Option<Box<Component>>,
    },
}

/// A text component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub content: Content,
    pub style: Style,
    pub extra: Vec<Component>,
}

impl Component {
    /// A component with the given contents and no style.
    pub fn new(content: Content) -> Self {
        Self {
            content,
            style: Style::default(),
            extra: Vec::new(),
        }
    }

    /// A literal text component. This is the only way player-supplied text enters a component.
    pub fn text(s: impl Into<String>) -> Self {
        Self::new(Content::Text(s.into()))
    }

    /// An empty literal (`Component.empty()`).
    pub fn empty() -> Self {
        Self::text("")
    }

    /// A translatable component with component arguments.
    pub fn translatable(key: impl Into<String>, args: Vec<Component>) -> Self {
        Self::translatable_args(key, args.into_iter().map(Arg::from).collect())
    }

    /// A translatable component with any arguments.
    pub fn translatable_args(key: impl Into<String>, args: Vec<Arg>) -> Self {
        Self::new(Content::Translatable {
            key: key.into(),
            fallback: None,
            args,
        })
    }

    /// A key binding component.
    pub fn keybind(name: impl Into<String>) -> Self {
        Self::new(Content::Keybind(name.into()))
    }

    /// A score component.
    pub fn score(name: impl Into<String>, objective: impl Into<String>) -> Self {
        Self::new(Content::Score {
            name: name.into(),
            objective: objective.into(),
        })
    }

    /// A selector component.
    pub fn selector(pattern: impl Into<String>, separator: Option<Component>) -> Self {
        Self::new(Content::Selector {
            pattern: pattern.into(),
            separator: separator.map(Box::new),
        })
    }

    /// Sets the fallback of a translatable component; no effect on other contents.
    #[must_use]
    pub fn fallback(mut self, text: impl Into<String>) -> Self {
        if let Content::Translatable { fallback, .. } = &mut self.content {
            *fallback = Some(text.into());
        }
        self
    }

    /// Replaces the style.
    #[must_use]
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Sets the colour.
    #[must_use]
    pub fn color(mut self, color: Color) -> Self {
        self.style.color = Some(color);
        self
    }

    /// Sets the ARGB shadow colour.
    #[must_use]
    pub fn shadow_color(mut self, argb: i32) -> Self {
        self.style.shadow_color = Some(argb);
        self
    }

    /// Sets bold.
    #[must_use]
    pub fn bold(mut self, on: bool) -> Self {
        self.style.bold = Some(on);
        self
    }

    /// Sets italic.
    #[must_use]
    pub fn italic(mut self, on: bool) -> Self {
        self.style.italic = Some(on);
        self
    }

    /// Sets underlined.
    #[must_use]
    pub fn underlined(mut self, on: bool) -> Self {
        self.style.underlined = Some(on);
        self
    }

    /// Sets strikethrough.
    #[must_use]
    pub fn strikethrough(mut self, on: bool) -> Self {
        self.style.strikethrough = Some(on);
        self
    }

    /// Sets obfuscated.
    #[must_use]
    pub fn obfuscated(mut self, on: bool) -> Self {
        self.style.obfuscated = Some(on);
        self
    }

    /// Sets the click event.
    #[must_use]
    pub fn click(mut self, event: ClickEvent) -> Self {
        self.style.click_event = Some(event);
        self
    }

    /// Sets the hover event.
    #[must_use]
    pub fn hover(mut self, event: HoverEvent) -> Self {
        self.style.hover_event = Some(Box::new(event));
        self
    }

    /// Sets the insertion (shift-click text).
    #[must_use]
    pub fn insertion(mut self, text: impl Into<String>) -> Self {
        self.style.insertion = Some(text.into());
        self
    }

    /// Sets the font.
    #[must_use]
    pub fn font(mut self, font: impl Into<String>) -> Self {
        self.style.font = Some(font.into());
        self
    }

    /// Appends a child.
    #[must_use]
    pub fn append(mut self, child: Component) -> Self {
        self.extra.push(child);
        self
    }

    /// `tryCollapseToString`: plain literal text without style and siblings.
    fn collapsed(&self) -> Option<&str> {
        match &self.content {
            Content::Text(t) if self.style.is_empty() && self.extra.is_empty() => Some(t),
            _ => None,
        }
    }

    /// The NBT form, byte-exact with Vanilla.
    pub fn to_nbt(&self) -> Tag {
        if let Some(t) = self.collapsed() {
            return Tag::String(t.to_owned());
        }
        let mut c = Compound::new();
        content_to_nbt(&self.content, &mut c);
        if !self.extra.is_empty() {
            c.insert("extra", List(self.extra.iter().map(Self::to_nbt).collect()));
        }
        style_to_nbt(&self.style, &mut c);
        Tag::Compound(c.in_java_hash_order())
    }

    /// The JSON form (a `serde_json` value; its maps are not ordered).
    pub fn to_json(&self) -> Value {
        json_value(&self.to_json_tree())
    }

    /// The JSON text in Vanilla's field order.
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.to_json_tree().write(&mut out);
        out
    }

    /// Plain text of literal parts, for logs.
    pub fn plain(&self) -> String {
        let mut out = String::new();
        self.collect_plain(&mut out);
        out
    }

    /// `Component.getString` with a language table (Vanilla's server uses `en_us`):
    /// translatable parts are filled in like `TranslatableContents.decomposeTemplate`; a
    /// missing key shows the fallback or the key.
    pub fn to_string_in(&self, language: &dyn Fn(&str) -> Option<String>) -> String {
        let mut out = String::new();
        self.collect_string(language, &mut out, 0);
        out
    }

    /// [`Self::to_string_in`] with the colours and formats as legacy `§` codes (`§c`, `§l`, hex
    /// colours as `§x§r§r§g§g§b§b`), for a console that shows them: never sent to clients.
    pub fn to_legacy_in(&self, language: &dyn Fn(&str) -> Option<String>) -> String {
        let mut out = String::new();
        let mut last = Legacy::default();
        self.collect_legacy(language, &mut out, 0, Legacy::default(), &mut last);
        out
    }

    fn collect_legacy(
        &self,
        language: &dyn Fn(&str) -> Option<String>,
        out: &mut String,
        depth: usize,
        parent: Legacy,
        last: &mut Legacy,
    ) {
        if depth > 64 {
            return;
        }
        let depth = depth.saturating_add(1);
        let own = parent.inherit(&self.style);
        match &self.content {
            Content::Translatable {
                key,
                fallback,
                args,
            } => {
                let format = language(key)
                    .or_else(|| fallback.clone())
                    .unwrap_or_else(|| key.clone());
                own.switch(last, out);
                let arg = |i: usize, out: &mut String| -> bool {
                    match args.get(i) {
                        Some(Arg::Component(c)) => {
                            c.collect_legacy(language, out, depth, own, last);
                            own.switch(last, out);
                        }
                        Some(arg) => push_scalar_arg(arg, out),
                        None => return false,
                    }
                    true
                };
                let mut filled = String::new();
                if fill_template(&format, &mut filled, arg) {
                    out.push_str(&filled);
                } else {
                    out.push_str(&format);
                }
            }
            Content::Text(t) | Content::Keybind(t) | Content::Selector { pattern: t, .. } => {
                if !t.is_empty() {
                    own.switch(last, out);
                    out.push_str(t);
                }
            }
            Content::Score { .. } | Content::Nbt { .. } | Content::Object { .. } => {}
        }
        for e in &self.extra {
            e.collect_legacy(language, out, depth, own, last);
        }
    }

    fn collect_string(
        &self,
        language: &dyn Fn(&str) -> Option<String>,
        out: &mut String,
        depth: usize,
    ) {
        // Arguments nest; a malicious depth is cut like elsewhere (SEC-04).
        if depth > 64 {
            return;
        }
        let depth = depth.saturating_add(1);
        match &self.content {
            Content::Translatable {
                key,
                fallback,
                args,
            } => {
                let format = language(key)
                    .or_else(|| fallback.clone())
                    .unwrap_or_else(|| key.clone());
                let arg = |i: usize, out: &mut String| -> bool {
                    match args.get(i) {
                        Some(Arg::Component(c)) => c.collect_string(language, out, depth),
                        Some(arg) => push_scalar_arg(arg, out),
                        None => return false,
                    }
                    true
                };
                let mut filled = String::new();
                if fill_template(&format, &mut filled, arg) {
                    out.push_str(&filled);
                } else {
                    // `TranslatableFormatException`: the raw template.
                    out.push_str(&format);
                }
            }
            Content::Text(t) => out.push_str(t),
            Content::Keybind(k) => out.push_str(k),
            Content::Selector { pattern, .. } => out.push_str(pattern),
            Content::Score { .. } | Content::Nbt { .. } | Content::Object { .. } => {}
        }
        for e in &self.extra {
            e.collect_string(language, out, depth);
        }
    }

    fn collect_plain(&self, out: &mut String) {
        match &self.content {
            Content::Text(t) => out.push_str(t),
            Content::Translatable { key, .. } => out.push_str(key),
            Content::Keybind(k) => out.push_str(k),
            Content::Selector { pattern, .. } => out.push_str(pattern),
            Content::Score { .. } | Content::Nbt { .. } | Content::Object { .. } => {}
        }
        for e in &self.extra {
            e.collect_plain(out);
        }
    }

    /// Decodes a component from NBT (`ComponentSerialization.CODEC`).
    pub fn try_from_nbt(tag: &Tag) -> Result<Self> {
        decode(&NbtIn(tag), 0)
    }

    /// Decodes a component from JSON.
    pub fn try_from_json(v: &Value) -> Result<Self> {
        decode(&JsonIn(v), 0)
    }

    /// Best-effort reading of a component from NBT: invalid input becomes its SNBT as text.
    pub fn from_nbt(tag: &Tag) -> Self {
        Self::try_from_nbt(tag).unwrap_or_else(|_| Self::text(vanillite_nbt::snbt::to_string(tag)))
    }

    /// Best-effort reading of a component from JSON: invalid input becomes its JSON as text.
    pub fn from_json(v: &Value) -> Self {
        Self::try_from_json(v).unwrap_or_else(|_| Self::text(v.to_string()))
    }
}

impl From<&str> for Component {
    fn from(s: &str) -> Self {
        Self::text(s)
    }
}

// ---------------------------------------------------------------------------------------------
// NBT encoding. Fields are inserted in codec order, then compounds are sorted like a HashMap.

fn bool_tag(v: bool) -> Tag {
    Tag::Byte(i8::from(v))
}

fn opt_component_nbt(c: &mut Compound, name: &str, v: &Option<Box<Component>>) {
    if let Some(v) = v {
        c.insert(name, v.to_nbt());
    }
}

fn content_to_nbt(content: &Content, c: &mut Compound) {
    match content {
        Content::Text(t) => c.insert("text", t.as_str()),
        Content::Translatable {
            key,
            fallback,
            args,
        } => {
            c.insert("translate", key.as_str());
            if let Some(f) = fallback {
                c.insert("fallback", f.as_str());
            }
            if !args.is_empty() {
                c.insert("with", List(args.iter().map(arg_to_nbt).collect()));
            }
        }
        Content::Score { name, objective } => {
            let inner = Compound::new()
                .with("name", name.as_str())
                .with("objective", objective.as_str())
                .in_java_hash_order();
            c.insert("score", inner);
        }
        Content::Selector { pattern, separator } => {
            c.insert("selector", pattern.as_str());
            opt_component_nbt(c, "separator", separator);
        }
        Content::Keybind(k) => c.insert("keybind", k.as_str()),
        Content::Nbt {
            path,
            interpret,
            plain,
            separator,
            source,
        } => {
            c.insert("nbt", path.as_str());
            if *interpret {
                c.insert("interpret", bool_tag(true));
            }
            if *plain {
                c.insert("plain", bool_tag(true));
            }
            opt_component_nbt(c, "separator", separator);
            match source {
                NbtSource::Entity(s) => c.insert("entity", s.as_str()),
                NbtSource::Block(s) => c.insert("block", s.as_str()),
                NbtSource::Storage(s) => c.insert("storage", s.as_str()),
            }
        }
        Content::Object { info, fallback } => {
            match info {
                ObjectInfo::Atlas { atlas, sprite } => {
                    if atlas != DEFAULT_ATLAS {
                        c.insert("atlas", atlas.as_str());
                    }
                    c.insert("sprite", sprite.as_str());
                }
                ObjectInfo::Player { player, hat } => {
                    c.insert("player", player.0.clone());
                    if !*hat {
                        c.insert("hat", bool_tag(false));
                    }
                }
            }
            opt_component_nbt(c, "fallback", fallback);
        }
    }
}

fn arg_to_nbt(arg: &Arg) -> Tag {
    match arg {
        Arg::Component(c) => c.to_nbt(),
        Arg::String(s) => Tag::String(s.clone()),
        Arg::Byte(v) => Tag::Byte(*v),
        Arg::Short(v) => Tag::Short(*v),
        Arg::Int(v) => Tag::Int(*v),
        Arg::Long(v) => Tag::Long(*v),
        Arg::Float(v) => Tag::Float(*v),
        Arg::Double(v) => Tag::Double(*v),
        Arg::Bool(v) => bool_tag(*v),
    }
}

fn style_to_nbt(s: &Style, c: &mut Compound) {
    if let Some(color) = s.color {
        c.insert("color", color.serialize());
    }
    if let Some(v) = s.shadow_color {
        c.insert("shadow_color", Tag::Int(v));
    }
    for (name, v) in [
        ("bold", s.bold),
        ("italic", s.italic),
        ("underlined", s.underlined),
        ("strikethrough", s.strikethrough),
        ("obfuscated", s.obfuscated),
    ] {
        if let Some(v) = v {
            c.insert(name, bool_tag(v));
        }
    }
    if let Some(e) = &s.click_event {
        c.insert("click_event", click_to_nbt(e));
    }
    if let Some(e) = &s.hover_event {
        c.insert("hover_event", hover_to_nbt(e));
    }
    if let Some(i) = &s.insertion {
        c.insert("insertion", i.as_str());
    }
    if let Some(f) = &s.font {
        c.insert("font", f.as_str());
    }
}

/// `KeyDispatchCodec` writes the value's fields first, then the action.
fn click_to_nbt(e: &ClickEvent) -> Tag {
    let mut c = Compound::new();
    match e {
        ClickEvent::OpenUrl(v) => c.insert("url", v.as_str()),
        ClickEvent::OpenFile(v) => c.insert("path", v.as_str()),
        ClickEvent::RunCommand(v) | ClickEvent::SuggestCommand(v) => {
            c.insert("command", v.as_str());
        }
        ClickEvent::ShowDialog(d) => c.insert("dialog", d.0.clone()),
        ClickEvent::ChangePage(p) => c.insert("page", Tag::Int(*p)),
        ClickEvent::CopyToClipboard(v) => c.insert("value", v.as_str()),
        ClickEvent::Custom { id, payload } => {
            c.insert("id", id.as_str());
            if let Some(p) = payload {
                c.insert("payload", p.0.clone());
            }
        }
    }
    c.insert("action", e.action());
    Tag::Compound(c.in_java_hash_order())
}

fn hover_to_nbt(e: &HoverEvent) -> Tag {
    let mut c = Compound::new();
    match e {
        HoverEvent::ShowText(t) => c.insert("value", t.to_nbt()),
        HoverEvent::ShowItem {
            id,
            count,
            components,
        } => {
            c.insert("id", id.as_str());
            if *count != 1 {
                c.insert("count", Tag::Int(*count));
            }
            if let Some(p) = components {
                c.insert("components", p.0.clone());
            }
        }
        HoverEvent::ShowEntity { id, uuid, name } => {
            c.insert("id", id.as_str());
            c.insert(
                "uuid",
                Tag::IntArray(vanillite_nbt::snbt::uuid_to_ints(*uuid).to_vec()),
            );
            if let Some(n) = name {
                c.insert("name", n.to_nbt());
            }
        }
    }
    c.insert("action", e.action());
    Tag::Compound(c.in_java_hash_order())
}

// ---------------------------------------------------------------------------------------------
// JSON encoding in codec order.

/// An ordered JSON tree (Gson keeps insertion order; `serde_json` maps here do not).
enum Json {
    Scalar(Value),
    /// A float or double as Gson writes it (`Float.toString`, `Double.toString`).
    Decimal(String),
    Array(Vec<Json>),
    Object(Vec<(&'static str, Json)>),
    /// An object with owned keys (opaque NBT compounds).
    OwnedObject(Vec<(String, Json)>),
}

impl Json {
    fn str(s: &str) -> Self {
        Self::Scalar(Value::String(s.to_owned()))
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Scalar(v) => out.push_str(&v.to_string()),
            Self::Decimal(v) => out.push_str(v),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String((*k).to_owned()).to_string());
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
            Self::OwnedObject(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn json_value(j: &Json) -> Value {
    match j {
        Json::Scalar(v) => v.clone(),
        Json::Decimal(v) => serde_json::from_str(v).unwrap_or(Value::Null),
        Json::Array(items) => Value::Array(items.iter().map(json_value).collect()),
        Json::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), json_value(v)))
                .collect::<Map<String, Value>>(),
        ),
        Json::OwnedObject(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), json_value(v)))
                .collect::<Map<String, Value>>(),
        ),
    }
}

/// A finite float in Java's form; others are `null` like `serde_json` writes them.
fn json_decimal(text: String, finite: bool) -> Json {
    if finite {
        Json::Decimal(text)
    } else {
        Json::Scalar(Value::Null)
    }
}

/// `NbtOps` → `JsonOps` conversion of opaque data.
fn tag_to_json(tag: &Tag) -> Json {
    let num = |n: Number| Json::Scalar(Value::Number(n));
    match tag {
        Tag::Byte(v) => num(Number::from(*v)),
        Tag::Short(v) => num(Number::from(*v)),
        Tag::Int(v) => num(Number::from(*v)),
        Tag::Long(v) => num(Number::from(*v)),
        Tag::Float(v) => json_decimal(java_float(*v), v.is_finite()),
        Tag::Double(v) => json_decimal(java_double(*v), v.is_finite()),
        Tag::String(s) => Json::str(s),
        Tag::ByteArray(a) => Json::Array(a.iter().map(|v| num(Number::from(*v))).collect()),
        Tag::IntArray(a) => Json::Array(a.iter().map(|v| num(Number::from(*v))).collect()),
        Tag::LongArray(a) => Json::Array(a.iter().map(|v| num(Number::from(*v))).collect()),
        Tag::List(l) => Json::Array(l.0.iter().map(tag_to_json).collect()),
        Tag::Compound(c) => Json::OwnedObject(
            c.iter()
                .map(|(k, v)| (k.to_owned(), tag_to_json(v)))
                .collect(),
        ),
    }
}

impl Component {
    fn to_json_tree(&self) -> Json {
        if let Some(t) = self.collapsed() {
            return Json::str(t);
        }
        let mut f: Vec<(&'static str, Json)> = Vec::new();
        content_to_json(&self.content, &mut f);
        if !self.extra.is_empty() {
            f.push((
                "extra",
                Json::Array(self.extra.iter().map(Self::to_json_tree).collect()),
            ));
        }
        style_to_json(&self.style, &mut f);
        Json::Object(f)
    }
}

fn content_to_json(content: &Content, f: &mut Vec<(&'static str, Json)>) {
    let opt = |f: &mut Vec<(&'static str, Json)>, name, v: &Option<Box<Component>>| {
        if let Some(v) = v {
            f.push((name, v.to_json_tree()));
        }
    };
    match content {
        Content::Text(t) => f.push(("text", Json::str(t))),
        Content::Translatable {
            key,
            fallback,
            args,
        } => {
            f.push(("translate", Json::str(key)));
            if let Some(fb) = fallback {
                f.push(("fallback", Json::str(fb)));
            }
            if !args.is_empty() {
                f.push(("with", Json::Array(args.iter().map(arg_to_json).collect())));
            }
        }
        Content::Score { name, objective } => f.push((
            "score",
            Json::Object(vec![
                ("name", Json::str(name)),
                ("objective", Json::str(objective)),
            ]),
        )),
        Content::Selector { pattern, separator } => {
            f.push(("selector", Json::str(pattern)));
            opt(f, "separator", separator);
        }
        Content::Keybind(k) => f.push(("keybind", Json::str(k))),
        Content::Nbt {
            path,
            interpret,
            plain,
            separator,
            source,
        } => {
            f.push(("nbt", Json::str(path)));
            if *interpret {
                f.push(("interpret", Json::Scalar(Value::Bool(true))));
            }
            if *plain {
                f.push(("plain", Json::Scalar(Value::Bool(true))));
            }
            opt(f, "separator", separator);
            match source {
                NbtSource::Entity(s) => f.push(("entity", Json::str(s))),
                NbtSource::Block(s) => f.push(("block", Json::str(s))),
                NbtSource::Storage(s) => f.push(("storage", Json::str(s))),
            }
        }
        Content::Object { info, fallback } => {
            match info {
                ObjectInfo::Atlas { atlas, sprite } => {
                    if atlas != DEFAULT_ATLAS {
                        f.push(("atlas", Json::str(atlas)));
                    }
                    f.push(("sprite", Json::str(sprite)));
                }
                ObjectInfo::Player { player, hat } => {
                    f.push(("player", tag_to_json(&player.0)));
                    if !*hat {
                        f.push(("hat", Json::Scalar(Value::Bool(false))));
                    }
                }
            }
            opt(f, "fallback", fallback);
        }
    }
}

fn arg_to_json(arg: &Arg) -> Json {
    let num = |n: Number| Json::Scalar(Value::Number(n));
    match arg {
        Arg::Component(c) => c.to_json_tree(),
        Arg::String(s) => Json::str(s),
        Arg::Byte(v) => num(Number::from(*v)),
        Arg::Short(v) => num(Number::from(*v)),
        Arg::Int(v) => num(Number::from(*v)),
        Arg::Long(v) => num(Number::from(*v)),
        Arg::Float(v) => json_decimal(java_float(*v), v.is_finite()),
        Arg::Double(v) => json_decimal(java_double(*v), v.is_finite()),
        Arg::Bool(v) => Json::Scalar(Value::Bool(*v)),
    }
}

fn style_to_json(s: &Style, f: &mut Vec<(&'static str, Json)>) {
    if let Some(color) = s.color {
        f.push(("color", Json::str(&color.serialize())));
    }
    if let Some(v) = s.shadow_color {
        f.push(("shadow_color", Json::Scalar(Value::Number(Number::from(v)))));
    }
    for (name, v) in [
        ("bold", s.bold),
        ("italic", s.italic),
        ("underlined", s.underlined),
        ("strikethrough", s.strikethrough),
        ("obfuscated", s.obfuscated),
    ] {
        if let Some(v) = v {
            f.push((name, Json::Scalar(Value::Bool(v))));
        }
    }
    if let Some(e) = &s.click_event {
        let mut c: Vec<(&'static str, Json)> = Vec::new();
        match e {
            ClickEvent::OpenUrl(v) => c.push(("url", Json::str(v))),
            ClickEvent::OpenFile(v) => c.push(("path", Json::str(v))),
            ClickEvent::RunCommand(v) | ClickEvent::SuggestCommand(v) => {
                c.push(("command", Json::str(v)));
            }
            ClickEvent::ShowDialog(d) => c.push(("dialog", tag_to_json(&d.0))),
            ClickEvent::ChangePage(p) => {
                c.push(("page", Json::Scalar(Value::Number(Number::from(*p)))));
            }
            ClickEvent::CopyToClipboard(v) => c.push(("value", Json::str(v))),
            ClickEvent::Custom { id, payload } => {
                c.push(("id", Json::str(id)));
                if let Some(p) = payload {
                    c.push(("payload", tag_to_json(&p.0)));
                }
            }
        }
        c.push(("action", Json::str(e.action())));
        f.push(("click_event", Json::Object(c)));
    }
    if let Some(e) = &s.hover_event {
        let mut c: Vec<(&'static str, Json)> = Vec::new();
        match e.as_ref() {
            HoverEvent::ShowText(t) => c.push(("value", t.to_json_tree())),
            HoverEvent::ShowItem {
                id,
                count,
                components,
            } => {
                c.push(("id", Json::str(id)));
                if *count != 1 {
                    c.push(("count", Json::Scalar(Value::Number(Number::from(*count)))));
                }
                if let Some(p) = components {
                    c.push(("components", tag_to_json(&p.0)));
                }
            }
            HoverEvent::ShowEntity { id, uuid, name } => {
                c.push(("id", Json::str(id)));
                let ints = vanillite_nbt::snbt::uuid_to_ints(*uuid);
                c.push((
                    "uuid",
                    Json::Array(
                        ints.iter()
                            .map(|v| Json::Scalar(Value::Number(Number::from(*v))))
                            .collect(),
                    ),
                ));
                if let Some(n) = name {
                    c.push(("name", n.to_json_tree()));
                }
            }
        }
        c.push(("action", Json::str(e.action())));
        f.push(("hover_event", Json::Object(c)));
    }
    if let Some(i) = &s.insertion {
        f.push(("insertion", Json::str(i)));
    }
    if let Some(font) = &s.font {
        f.push(("font", Json::str(font)));
    }
}

// ---------------------------------------------------------------------------------------------
// Decoding: one decoder over NBT and JSON input through a small view trait.

/// A read-only view of a decoded value (NBT tag or JSON value).
trait Input: Sized {
    fn as_str(&self) -> Option<&str>;
    /// A number as `i64` (any integral or floating value, truncated like `Number.intValue`).
    fn as_i64(&self) -> Option<i64>;
    /// `getBooleanValue`: a boolean or a number.
    fn as_bool(&self) -> Option<bool>;
    /// List elements.
    fn as_list(&self) -> Option<Vec<Self>>;
    /// A field of a map.
    fn field(&self, name: &str) -> Option<Self>;
    fn is_map(&self) -> bool;
    /// A UUID: an int array of 4 or a UUID string (`UUIDUtil.LENIENT_CODEC`).
    fn as_uuid(&self) -> Option<u128>;
    /// The value as opaque NBT.
    fn to_tag(&self) -> Option<Tag>;
    /// A translation argument primitive, if this is one.
    fn as_primitive(&self) -> Option<Arg>;
}

#[derive(Clone, Copy)]
struct NbtIn<'a>(&'a Tag);

impl Input for NbtIn<'_> {
    fn as_str(&self) -> Option<&str> {
        self.0.as_str()
    }

    fn as_i64(&self) -> Option<i64> {
        match self.0 {
            Tag::Byte(v) => Some(i64::from(*v)),
            Tag::Short(v) => Some(i64::from(*v)),
            Tag::Int(v) => Some(i64::from(*v)),
            Tag::Long(v) => Some(*v),
            Tag::Float(v) => float_to_i64(f64::from(*v)),
            Tag::Double(v) => float_to_i64(*v),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        self.as_i64().map(|v| v & 0xFF != 0)
    }

    fn as_list(&self) -> Option<Vec<Self>> {
        match self.0 {
            Tag::List(l) => Some(l.0.iter().map(NbtIn).collect()),
            _ => None,
        }
    }

    fn field(&self, name: &str) -> Option<Self> {
        match self.0 {
            Tag::Compound(c) => c.get(name).map(NbtIn),
            _ => None,
        }
    }

    fn is_map(&self) -> bool {
        matches!(self.0, Tag::Compound(_))
    }

    fn as_uuid(&self) -> Option<u128> {
        match self.0 {
            Tag::IntArray(a) => ints_to_uuid(a),
            Tag::List(l) => {
                let ints: Option<Vec<i32>> =
                    l.0.iter()
                        .map(|t| match t {
                            Tag::Int(v) => Some(*v),
                            _ => None,
                        })
                        .collect();
                ints_to_uuid(&ints?)
            }
            Tag::String(s) => parse_uuid(s),
            _ => None,
        }
    }

    fn to_tag(&self) -> Option<Tag> {
        Some(self.0.clone())
    }

    fn as_primitive(&self) -> Option<Arg> {
        Some(match self.0 {
            Tag::String(s) => Arg::String(s.clone()),
            Tag::Byte(v) => Arg::Byte(*v),
            Tag::Short(v) => Arg::Short(*v),
            Tag::Int(v) => Arg::Int(*v),
            Tag::Long(v) => Arg::Long(*v),
            Tag::Float(v) => Arg::Float(*v),
            Tag::Double(v) => Arg::Double(*v),
            _ => return None,
        })
    }
}

#[derive(Clone, Copy)]
struct JsonIn<'a>(&'a Value);

impl Input for JsonIn<'_> {
    fn as_str(&self) -> Option<&str> {
        self.0.as_str()
    }

    fn as_i64(&self) -> Option<i64> {
        self.0
            .as_i64()
            .or_else(|| self.0.as_f64().and_then(float_to_i64))
    }

    fn as_bool(&self) -> Option<bool> {
        self.0
            .as_bool()
            .or_else(|| self.as_i64().map(|v| v & 0xFF != 0))
    }

    fn as_list(&self) -> Option<Vec<Self>> {
        self.0.as_array().map(|a| a.iter().map(JsonIn).collect())
    }

    fn field(&self, name: &str) -> Option<Self> {
        self.0.get(name).map(JsonIn)
    }

    fn is_map(&self) -> bool {
        self.0.is_object()
    }

    fn as_uuid(&self) -> Option<u128> {
        match self.0 {
            Value::Array(a) => {
                let ints: Option<Vec<i32>> = a
                    .iter()
                    .map(|v| v.as_i64().and_then(|v| i32::try_from(v).ok()))
                    .collect();
                ints_to_uuid(&ints?)
            }
            Value::String(s) => parse_uuid(s),
            _ => None,
        }
    }

    fn to_tag(&self) -> Option<Tag> {
        json_to_tag(self.0, 0)
    }

    fn as_primitive(&self) -> Option<Arg> {
        Some(match self.0 {
            Value::String(s) => Arg::String(s.clone()),
            Value::Bool(b) => Arg::Bool(*b),
            Value::Number(n) => match n.as_i64() {
                Some(v) => i32::try_from(v).map_or(Arg::Long(v), Arg::Int),
                None => Arg::Double(n.as_f64()?),
            },
            _ => return None,
        })
    }
}

/// `JsonOps` → `NbtOps` conversion of opaque data (bounded by [`MAX_DEPTH`]).
fn json_to_tag(v: &Value, depth: usize) -> Option<Tag> {
    if depth > MAX_DEPTH {
        return None;
    }
    let depth = depth.saturating_add(1);
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => bool_tag(*b),
        Value::Number(n) => match n.as_i64() {
            Some(v) => i32::try_from(v).map_or(Tag::Long(v), Tag::Int),
            None => Tag::Double(n.as_f64()?),
        },
        Value::String(s) => Tag::String(s.clone()),
        Value::Array(a) => Tag::List(List(
            a.iter()
                .map(|v| json_to_tag(v, depth))
                .collect::<Option<Vec<_>>>()?,
        )),
        Value::Object(m) => {
            let mut c = Compound::new();
            for (k, v) in m {
                c.insert(k.as_str(), json_to_tag(v, depth)?);
            }
            Tag::Compound(c)
        }
    })
}

fn float_to_i64(v: f64) -> Option<i64> {
    // Java's `(int)`/`(long)` truncation; out-of-range values are rejected.
    let t = v.trunc();
    (-9.2e18..=9.2e18)
        .contains(&t)
        .then(|| format!("{t:.0}").parse().ok())?
}

fn ints_to_uuid(a: &[i32]) -> Option<u128> {
    let [a, b, c, d] = <[i32; 4]>::try_from(a).ok()?;
    let part = |v: i32| u128::from(u32::from_ne_bytes(v.to_ne_bytes()));
    Some((part(a) << 96) | (part(b) << 64) | (part(c) << 32) | part(d))
}

/// A UUID in the dashed form `UUID.fromString` accepts.
fn parse_uuid(s: &str) -> Option<u128> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 || parts.iter().any(|p| p.is_empty() || p.len() > 16) {
        return None;
    }
    let widths = [32u32, 16, 16, 16, 48];
    let mut v: u128 = 0;
    for (p, w) in parts.iter().zip(widths) {
        let n = u128::from(u64::from_str_radix(p, 16).ok()?);
        let max = (1u128 << w).wrapping_sub(1);
        v = (v << w) | (n & max);
    }
    Some(v)
}

/// `Identifier.CODEC`: validates and adds the default namespace.
fn identifier(s: &str) -> Result<String> {
    let (ns, path) = s.split_once(':').unwrap_or(("minecraft", s));
    let ns_ok = ns
        .chars()
        .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-' | '.'));
    let path_ok = path
        .chars()
        .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-' | '.' | '/'));
    if !ns_ok || !path_ok {
        return Err(TextError::new("invalid identifier"));
    }
    let ns = if ns.is_empty() { "minecraft" } else { ns };
    Ok(format!("{ns}:{path}"))
}

/// `ExtraCodecs.CHAT_STRING`: no `§`, no control characters, no DEL.
fn chat_string(s: &str) -> Result<String> {
    if s.chars().any(|c| c == '§' || c < ' ' || c == '\u{7f}') {
        return Err(TextError::new("illegal characters in command"));
    }
    Ok(s.to_owned())
}

fn req_str<I: Input>(v: &I, name: &str) -> Result<String> {
    v.field(name)
        .and_then(|f| f.as_str().map(str::to_owned))
        .ok_or(TextError::new("missing string field"))
}

/// `optionalFieldOf`: absent is fine, present but invalid is an error.
fn opt<I: Input, T>(v: &I, name: &str, f: impl FnOnce(&I) -> Option<T>) -> Result<Option<T>> {
    match v.field(name) {
        None => Ok(None),
        Some(x) => f(&x)
            .map(Some)
            .ok_or(TextError::new("invalid optional field")),
    }
}

#[inline(never)]
fn opt_component<I: Input>(v: &I, name: &str, depth: usize) -> Result<Option<Box<Component>>> {
    match v.field(name) {
        None => Ok(None),
        Some(x) => decode(&x, depth).map(|c| Some(Box::new(c))),
    }
}

fn decode<I: Input>(v: &I, depth: usize) -> Result<Component> {
    let mut out = Vec::with_capacity(1);
    decode_into(v, depth, &mut out)?;
    out.pop().ok_or(TextError::new("not a component"))
}

/// Decodes one component and pushes it to `out`. Recursion goes through this small frame only,
/// so nesting up to [`MAX_DEPTH`] fits on a network thread's stack.
fn decode_into<I: Input>(v: &I, depth: usize, out: &mut Vec<Component>) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(TextError::new("too deeply nested"));
    }
    let next = depth.saturating_add(1);
    if let Some(s) = v.as_str() {
        out.push(Component::text(s));
        return Ok(());
    }
    if let Some(items) = v.as_list() {
        // `createFromList`: the first element with the others appended.
        let mut parts = Vec::with_capacity(items.len());
        for item in &items {
            decode_into(item, next, &mut parts)?;
        }
        let mut parts = parts.into_iter();
        let mut first = parts.next().ok_or(TextError::new("empty component list"))?;
        first.extra.extend(parts);
        out.push(first);
        return Ok(());
    }
    if !v.is_map() {
        return Err(TextError::new("not a component"));
    }
    let content = Box::new(decode_content(v, next)?);
    let mut extra = Vec::new();
    if let Some(e) = v.field("extra") {
        let items = e.as_list().ok_or(TextError::new("extra is not a list"))?;
        if items.is_empty() {
            return Err(TextError::new("extra is empty"));
        }
        extra.reserve(items.len());
        for item in &items {
            decode_into(item, next, &mut extra)?;
        }
    }
    let mut style = Box::new(decode_style(v)?);
    if let Some(e) = v.field("hover_event") {
        style.hover_event = Some(decode_hover(&e, next)?);
    }
    out.push(Component {
        content: *content,
        style: *style,
        extra,
    });
    Ok(())
}

/// Content types in `ComponentSerialization.bootstrap` order (the order of the fuzzy match).
const CONTENT_TYPES: [&str; 7] = [
    "text",
    "translatable",
    "keybind",
    "score",
    "selector",
    "nbt",
    "object",
];

#[inline(never)]
fn decode_content<I: Input>(v: &I, depth: usize) -> Result<Content> {
    if let Some(t) = v.field("type") {
        let name = t.as_str().ok_or(TextError::new("type is not a string"))?;
        if !CONTENT_TYPES.contains(&name) {
            return Err(TextError::new("unknown content type"));
        }
        return decode_content_of(v, name, depth);
    }
    CONTENT_TYPES
        .iter()
        .find_map(|name| decode_content_of(v, name, depth).ok())
        .ok_or(TextError::new("no content"))
}

#[inline(never)]
fn decode_content_of<I: Input>(v: &I, kind: &str, depth: usize) -> Result<Content> {
    Ok(match kind {
        "text" => Content::Text(req_str(v, "text")?),
        "translatable" => Content::Translatable {
            key: req_str(v, "translate")?,
            // `lenientOptionalFieldOf`: invalid counts as absent.
            fallback: v
                .field("fallback")
                .and_then(|f| f.as_str().map(str::to_owned)),
            args: decode_args(v, depth)?,
        },
        "keybind" => Content::Keybind(req_str(v, "keybind")?),
        "score" => {
            let s = v.field("score").ok_or(TextError::new("missing score"))?;
            Content::Score {
                name: req_str(&s, "name")?,
                objective: req_str(&s, "objective")?,
            }
        }
        "selector" => Content::Selector {
            pattern: req_str(v, "selector")?,
            separator: opt_component(v, "separator", depth)?,
        },
        "nbt" => {
            let lenient_bool = |name| v.field(name).and_then(|f| f.as_bool()).unwrap_or(false);
            let interpret = lenient_bool("interpret");
            let plain = lenient_bool("plain");
            if interpret && plain {
                return Err(TextError::new("interpret and plain are both on"));
            }
            Content::Nbt {
                path: req_str(v, "nbt")?,
                interpret,
                plain,
                separator: opt_component(v, "separator", depth).unwrap_or(None),
                source: decode_nbt_source(v)?,
            }
        }
        "object" => Content::Object {
            info: decode_object(v)?,
            fallback: opt_component(v, "fallback", depth)?,
        },
        _ => return Err(TextError::new("unknown content type")),
    })
}

#[inline(never)]
fn decode_nbt_source<I: Input>(v: &I) -> Result<NbtSource> {
    let kinds = ["entity", "block", "storage"];
    let of = |kind: &str| -> Result<NbtSource> {
        Ok(match kind {
            "entity" => NbtSource::Entity(req_str(v, "entity")?),
            "block" => NbtSource::Block(req_str(v, "block")?),
            "storage" => NbtSource::Storage(identifier(&req_str(v, "storage")?)?),
            _ => return Err(TextError::new("unknown nbt source")),
        })
    };
    match v.field("source") {
        Some(s) => of(s.as_str().ok_or(TextError::new("source is not a string"))?),
        None => kinds
            .iter()
            .find_map(|k| of(k).ok())
            .ok_or(TextError::new("no nbt source")),
    }
}

#[inline(never)]
fn decode_object<I: Input>(v: &I) -> Result<ObjectInfo> {
    let of = |kind: &str| -> Result<ObjectInfo> {
        Ok(match kind {
            "atlas" => ObjectInfo::Atlas {
                atlas: match opt(v, "atlas", |a| a.as_str().map(str::to_owned))? {
                    Some(a) => identifier(&a)?,
                    None => DEFAULT_ATLAS.to_owned(),
                },
                sprite: identifier(&req_str(v, "sprite")?)?,
            },
            "player" => ObjectInfo::Player {
                player: OpaqueNbt(
                    v.field("player")
                        .and_then(|p| p.to_tag())
                        .ok_or(TextError::new("missing player"))?,
                ),
                hat: opt(v, "hat", Input::as_bool)?.unwrap_or(true),
            },
            _ => return Err(TextError::new("unknown object type")),
        })
    };
    match v.field("object") {
        Some(t) => of(t.as_str().ok_or(TextError::new("object is not a string"))?),
        None => ["atlas", "player"]
            .iter()
            .find_map(|k| of(k).ok())
            .ok_or(TextError::new("no object")),
    }
}

/// The `with` list. Each argument is `TranslatableContents.ARG_CODEC`: a primitive, or a
/// component that collapses to a string when it is plain text.
#[inline(never)]
fn decode_args<I: Input>(v: &I, depth: usize) -> Result<Vec<Arg>> {
    let Some(w) = v.field("with") else {
        return Ok(Vec::new());
    };
    let items = w.as_list().ok_or(TextError::new("with is not a list"))?;
    let mut args = Vec::with_capacity(items.len());
    let mut parts = Vec::with_capacity(1);
    for item in &items {
        if let Some(p) = item.as_primitive() {
            args.push(p);
            continue;
        }
        decode_into(item, depth, &mut parts)?;
        if let Some(c) = parts.pop() {
            args.push(match c.collapsed() {
                Some(t) => Arg::String(t.to_owned()),
                None => Arg::from(c),
            });
        }
    }
    Ok(args)
}

/// `TextColor.parseColor`'s error.
#[cold]
fn invalid_color(color: &str) -> TextError {
    TextError(std::borrow::Cow::Owned(match color.strip_prefix('#') {
        Some(hex) if i32::from_str_radix(hex, 16).is_ok() => {
            format!("Color value out of range: {color}")
        }
        Some(_) => format!("Invalid color value: {color}"),
        None => format!("Invalid color name: {color}"),
    }))
}

#[inline(never)]
fn decode_style<I: Input>(v: &I) -> Result<Style> {
    let color = match opt(v, "color", |c| c.as_str().map(str::to_owned))? {
        Some(c) => Some(Color::parse(&c).ok_or_else(|| invalid_color(&c))?),
        None => None,
    };
    let int = |x: &I| x.as_i64().and_then(|v| i32::try_from(v).ok());
    Ok(Style {
        color,
        shadow_color: opt(v, "shadow_color", int)?,
        bold: opt(v, "bold", Input::as_bool)?,
        italic: opt(v, "italic", Input::as_bool)?,
        underlined: opt(v, "underlined", Input::as_bool)?,
        strikethrough: opt(v, "strikethrough", Input::as_bool)?,
        obfuscated: opt(v, "obfuscated", Input::as_bool)?,
        click_event: match v.field("click_event") {
            Some(e) => Some(decode_click(&e)?),
            None => None,
        },
        // Decoded by the caller, outside this large frame.
        hover_event: None,
        insertion: opt(v, "insertion", |c| c.as_str().map(str::to_owned))?,
        font: match opt(v, "font", |c| c.as_str().map(str::to_owned))? {
            Some(f) => Some(identifier(&f)?),
            None => None,
        },
    })
}

#[inline(never)]
fn decode_click<I: Input>(v: &I) -> Result<ClickEvent> {
    let action = req_str(v, "action")?;
    Ok(match action.as_str() {
        "open_url" => {
            let url = req_str(v, "url")?;
            let lower = url.to_ascii_lowercase();
            if !lower.starts_with("http://") && !lower.starts_with("https://") {
                return Err(TextError::new("unsupported URL scheme"));
            }
            ClickEvent::OpenUrl(url)
        }
        // `open_file` is not allowed from servers (`filterForSerialization`).
        "run_command" => ClickEvent::RunCommand(chat_string(&req_str(v, "command")?)?),
        "suggest_command" => ClickEvent::SuggestCommand(chat_string(&req_str(v, "command")?)?),
        "show_dialog" => ClickEvent::ShowDialog(OpaqueNbt(
            v.field("dialog")
                .and_then(|d| d.to_tag())
                .ok_or(TextError::new("missing dialog"))?,
        )),
        "change_page" => ClickEvent::ChangePage(
            v.field("page")
                .and_then(|p| p.as_i64())
                .and_then(|p| i32::try_from(p).ok())
                .filter(|p| *p >= 1)
                .ok_or(TextError::new("invalid page"))?,
        ),
        "copy_to_clipboard" => ClickEvent::CopyToClipboard(req_str(v, "value")?),
        "custom" => ClickEvent::Custom {
            id: identifier(&req_str(v, "id")?)?,
            payload: opt(v, "payload", |p| p.to_tag())?.map(OpaqueNbt),
        },
        _ => return Err(TextError::new("unknown click action")),
    })
}

#[inline(never)]
fn decode_hover<I: Input>(v: &I, depth: usize) -> Result<Box<HoverEvent>> {
    let action = req_str(v, "action")?;
    let mut parts = Vec::with_capacity(1);
    match action.as_str() {
        "show_text" => {
            let value = v.field("value").ok_or(TextError::new("missing value"))?;
            decode_into(&value, depth, &mut parts)?;
            let text = parts.pop().ok_or(TextError::new("missing value"))?;
            Ok(Box::new(HoverEvent::ShowText(text)))
        }
        "show_entity" => {
            let (id, uuid) = decode_entity_tooltip(v)?;
            if let Some(name) = v.field("name") {
                decode_into(&name, depth, &mut parts)?;
            }
            Ok(Box::new(HoverEvent::ShowEntity {
                id,
                uuid,
                name: parts.pop(),
            }))
        }
        "show_item" => decode_item_tooltip(v),
        _ => Err(TextError::new("unknown hover action")),
    }
}

#[inline(never)]
fn decode_entity_tooltip<I: Input>(v: &I) -> Result<(String, u128)> {
    Ok((
        identifier(&req_str(v, "id")?)?,
        v.field("uuid")
            .and_then(|u| u.as_uuid())
            .ok_or(TextError::new("invalid uuid"))?,
    ))
}

#[inline(never)]
fn decode_item_tooltip<I: Input>(v: &I) -> Result<Box<HoverEvent>> {
    Ok(Box::new(HoverEvent::ShowItem {
        id: identifier(&req_str(v, "id")?)?,
        count: match opt(v, "count", |c| c.as_i64())? {
            Some(c) => i32::try_from(c)
                .ok()
                .filter(|c| (1..=99).contains(c))
                .ok_or(TextError::new("invalid count"))?,
            None => 1,
        },
        components: opt(v, "components", |c| c.to_tag())?
            .filter(|t| !matches!(t, Tag::Compound(c) if c.is_empty()))
            .map(OpaqueNbt),
    }))
}

// ---------------------------------------------------------------------------------------------
// Wire forms.

/// Wire form of a component in configuration and play: network NBT.
impl Encode for Component {
    fn encode(&self, out: &mut Vec<u8>) {
        crate::types::Nbt(Some(self.to_nbt())).encode(out);
    }
}

/// A component written as a JSON string (status, login disconnect).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonComponent(pub Component);

impl Encode for JsonComponent {
    fn encode(&self, out: &mut Vec<u8>) {
        self.0.to_json_string().encode(out);
    }
}

/// Components from the network are read with the given NBT quota; an invalid component is an
/// error.
impl crate::codec::DecodeLimited for Component {
    fn decode_limited(r: &mut crate::codec::Reader<'_>, max: usize) -> crate::codec::Result<Self> {
        let nbt = crate::types::Nbt::decode_limited(r, max)?;
        match nbt.0.as_ref() {
            Some(tag) => {
                Self::try_from_nbt(tag).map_err(|_| crate::codec::Error::Invalid("text component"))
            }
            None => Err(crate::codec::Error::Invalid("empty text component")),
        }
    }
}

/// JSON components are read as a string of at most `max` characters.
impl crate::codec::DecodeLimited for JsonComponent {
    fn decode_limited(r: &mut crate::codec::Reader<'_>, max: usize) -> crate::codec::Result<Self> {
        let s = r.string(max)?;
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|_| crate::codec::Error::Invalid("JSON component"))?;
        Component::try_from_json(&v)
            .map(Self)
            .map_err(|_| crate::codec::Error::Invalid("JSON component"))
    }
}

#[cfg(test)]
mod string_tests {
    use super::*;

    #[test]
    fn fills_templates_like_vanilla() {
        let lang = |key: &str| match key {
            "commands.list.players" => {
                Some("There are %s of a max of %s players online: %s".to_owned())
            }
            "swap" => Some("%2$s then %1$s, 100%%".to_owned()),
            "bad" => Some("%d".to_owned()),
            _ => None,
        };
        let list = Component::translatable_args(
            "commands.list.players",
            vec![Arg::Int(0), Arg::Int(20), Arg::String(String::new())],
        );
        assert_eq!(
            list.to_string_in(&lang),
            "There are 0 of a max of 20 players online: "
        );
        let swap = Component::translatable_args(
            "swap",
            vec![Arg::String("a".into()), Arg::String("b".into())],
        );
        assert_eq!(swap.to_string_in(&lang), "b then a, 100%");
        assert_eq!(
            Component::translatable("bad", vec![]).to_string_in(&lang),
            "%d"
        );
        assert_eq!(
            Component::translatable("missing", vec![]).to_string_in(&lang),
            "missing"
        );
    }
}
