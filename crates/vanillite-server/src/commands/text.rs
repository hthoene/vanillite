//! Text components with server-side contents resolved for a command source (Vanilla's
//! `ComponentUtils.resolve` with a `ResolutionContext`): entity selectors, scores and NBT.

use vanillite_command::error::CommandError;
use vanillite_command::reader::StringReader;
use vanillite_command::selector::SelectorParser;
use vanillite_game::scoreboard::NumberFormat;
use vanillite_nbt::Tag;
use vanillite_protocol::text::{Arg, Component, Content, HoverEvent, NbtSource, Style};

use super::Env;
use super::exec::format_list;
use super::select::{self, Target};
use super::source::CommandSource;

/// `ResolutionContext`'s default depth limit; deeper parts are copied.
const DEPTH_LIMIT: u32 = 100;
/// `DEFAULT_RESOLVED_COMPONENT_LIMIT`: NBT contents resolved per component.
const NBT_LIMIT: u32 = 65_536;

struct Context<'a, 'e> {
    env: &'a Env<'e>,
    source: &'a CommandSource,
    /// `defaultScoreboardEntity`: the score holder of `*` in score contents.
    holder: Option<String>,
    nbt_count: u32,
}

/// `ComponentUtils.resolve` with `withSource(source)` and, when given, `withEntityOverride`.
pub(crate) fn resolve(
    env: &Env<'_>,
    source: &CommandSource,
    component: &Component,
    entity: Option<&Target>,
) -> Result<Component, CommandError> {
    let holder = match entity {
        Some(t) => Some(t.score_name()),
        None => source.entity.as_ref().map(|e| {
            if e.is_player() {
                e.name.clone()
            } else {
                vanillite_protocol::types::Uuid(e.uuid).to_string()
            }
        }),
    };
    let mut ctx = Context {
        env,
        source,
        holder,
        nbt_count: 0,
    };
    ctx.resolve(component, 0)
}

/// `Style.applyTo`: the attributes of `over`, the others of `under`.
fn apply_to(over: &Style, under: &Style) -> Style {
    Style {
        color: over.color.or(under.color),
        shadow_color: over.shadow_color.or(under.shadow_color),
        bold: over.bold.or(under.bold),
        italic: over.italic.or(under.italic),
        underlined: over.underlined.or(under.underlined),
        strikethrough: over.strikethrough.or(under.strikethrough),
        obfuscated: over.obfuscated.or(under.obfuscated),
        click_event: over
            .click_event
            .clone()
            .or_else(|| under.click_event.clone()),
        hover_event: over
            .hover_event
            .clone()
            .or_else(|| under.hover_event.clone()),
        insertion: over.insertion.clone().or_else(|| under.insertion.clone()),
        font: over.font.clone().or_else(|| under.font.clone()),
    }
}

impl Context<'_, '_> {
    fn resolve(&mut self, component: &Component, depth: u32) -> Result<Component, CommandError> {
        if depth > DEPTH_LIMIT {
            return Ok(component.clone());
        }
        let inner = depth.saturating_add(1);
        let mut out = self.contents(&component.content, inner)?;
        for sibling in &component.extra {
            let resolved = self.resolve(sibling, inner)?;
            out.extra.push(resolved);
        }
        let mut style = component.style.clone();
        if let Some(hover) = style.hover_event.as_deref()
            && let HoverEvent::ShowText(text) = hover
        {
            let text = self.resolve(text, inner)?;
            style.hover_event = Some(Box::new(HoverEvent::ShowText(text)));
        }
        out.style = apply_to(&style, &out.style);
        Ok(out)
    }

    fn separator(
        &mut self,
        separator: Option<&Component>,
        depth: u32,
    ) -> Result<Option<Component>, CommandError> {
        separator.map(|s| self.resolve(s, depth)).transpose()
    }

    /// `ComponentContents.resolve`.
    fn contents(&mut self, content: &Content, depth: u32) -> Result<Component, CommandError> {
        match content {
            Content::Translatable {
                key,
                fallback,
                args,
            } => {
                let mut resolved = Vec::with_capacity(args.len());
                for arg in args {
                    resolved.push(match arg {
                        Arg::Component(c) => Arg::from(self.resolve(c, depth)?),
                        other => other.clone(),
                    });
                }
                Ok(Component::new(Content::Translatable {
                    key: key.clone(),
                    fallback: fallback.clone(),
                    args: resolved,
                }))
            }
            Content::Selector { pattern, separator } => {
                let separator = self.separator(separator.as_deref(), depth)?;
                let names = self
                    .select(pattern)?
                    .iter()
                    .map(|t| t.display(self.env.global))
                    .collect();
                Ok(join(names, separator))
            }
            Content::Score { name, objective } => self.score(name, objective),
            Content::Nbt {
                path,
                interpret,
                separator,
                source,
                ..
            } => self.nbt(path, *interpret, separator.as_deref(), source, depth),
            other => Ok(Component::new(other.clone())),
        }
    }

    /// The entities of a selector pattern (`EntitySelector.findEntities`).
    fn select(&self, pattern: &str) -> Result<Vec<Target>, CommandError> {
        let commands = self.env.global.commands();
        let limits = commands.limits();
        let mut reader = StringReader::new(pattern);
        let selector = SelectorParser::new(&mut reader, true, limits).parse()?;
        select::optional(self.env, self.source, &selector)
    }

    /// `ScoreContents.resolve`.
    fn score(&mut self, name: &str, objective: &str) -> Result<Component, CommandError> {
        let holder = if name.starts_with('@') {
            let found = self.select(name)?;
            match found.as_slice() {
                [] => name.to_owned(),
                [one] => one.score_name(),
                _ => return Err(CommandError::new("argument.entity.toomany", vec![])),
            }
        } else if name == "*" {
            self.holder.clone().unwrap_or_else(|| name.to_owned())
        } else {
            name.to_owned()
        };
        let board = self.env.global.systems.scoreboard();
        let Some(objective) = board.objective(objective) else {
            return Ok(Component::empty());
        };
        let Some(score) = board.score(&holder, &objective.name) else {
            return Ok(Component::empty());
        };
        // `ReadOnlyScoreInfo.formatValue(objective.numberFormatOrDefault(NO_STYLE))`.
        let format = score
            .number_format
            .as_ref()
            .or(objective.number_format.as_ref());
        Ok(match format {
            Some(NumberFormat::Blank) => Component::empty(),
            Some(NumberFormat::Fixed(c)) => (**c).clone(),
            Some(NumberFormat::Styled(style)) => {
                Component::text(score.value.to_string()).style((**style).clone())
            }
            None => Component::text(score.value.to_string()),
        })
    }

    /// `NbtContents.resolve` for storage and entities; values are shown as plain SNBT.
    fn nbt(
        &mut self,
        path: &str,
        interpret: bool,
        separator: Option<&Component>,
        source: &NbtSource,
        depth: u32,
    ) -> Result<Component, CommandError> {
        if self.nbt_count > NBT_LIMIT {
            return Ok(Component::empty());
        }
        self.nbt_count = self.nbt_count.saturating_add(1);
        let commands = self.env.global.commands();
        let limits = commands.limits();
        let Ok(path) = vanillite_command::nbt_path::parse(&mut StringReader::new(path), limits)
        else {
            return Ok(Component::empty());
        };
        let roots: Vec<Tag> = match source {
            NbtSource::Storage(id) => {
                let id = if id.contains(':') {
                    id.clone()
                } else {
                    format!("minecraft:{id}")
                };
                let storage = self.env.global.systems.storage();
                vec![Tag::Compound(storage.get(&id).cloned().unwrap_or_default())]
            }
            NbtSource::Entity(pattern) => self
                .select(pattern)?
                .iter()
                .filter_map(|t| t.entity)
                .filter_map(|id| super::data::entity_nbt(&self.env.level.entities, id))
                .map(Tag::Compound)
                .collect(),
            // Block entity data is not readable by commands yet.
            NbtSource::Block(_) => Vec::new(),
        };
        let values: Vec<Tag> = roots
            .iter()
            .filter_map(|root| super::nbt_ops::get(&path, root).ok())
            .flatten()
            .collect();
        let separator = self
            .separator(separator, depth)?
            .unwrap_or_else(|| Component::text(", "));
        let mut parts = Vec::with_capacity(values.len());
        for value in &values {
            if interpret {
                if let Ok(c) = Component::try_from_nbt(value) {
                    parts.push(self.resolve(&c, depth)?);
                }
            } else {
                parts.push(Component::text(vanillite_nbt::snbt::to_string(value)));
            }
        }
        Ok(join(parts, Some(separator)))
    }
}

/// `ComponentUtils.formatList` with an optional separator (default: gray `, `).
fn join(parts: Vec<Component>, separator: Option<Component>) -> Component {
    let Some(separator) = separator else {
        return format_list(parts);
    };
    let mut parts = parts.into_iter();
    let Some(first) = parts.next() else {
        return Component::empty();
    };
    let mut rest = parts.peekable();
    if rest.peek().is_none() {
        return first;
    }
    let mut out = Component::empty().append(first);
    for part in rest {
        out = out.append(separator.clone()).append(part);
    }
    out
}
