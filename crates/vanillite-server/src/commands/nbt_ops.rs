//! Reading and changing NBT along a path (Vanilla's `NbtPathArgument.NbtPath` and its nodes,
//! `NbtUtils.compareNbt`).

use vanillite_command::Arg as ErrArg;
use vanillite_command::error::CommandError;
use vanillite_command::nbt_path::{NbtPath, PathNode};
use vanillite_nbt::{Compound, List, Tag};

/// Vanilla's depth limit of data written with a path.
const MAX_DEPTH: usize = 512;

/// `NbtUtils.compareNbt(expected, actual, true)`.
pub(crate) use vanillite_game::item_nbt::compare_nbt as matches;

/// `compareNbt` of two compounds.
pub(crate) use vanillite_game::item_nbt::compare_compounds as compound_matches;

/// The elements of a collection tag (lists and arrays).
fn elements(tag: &Tag) -> Option<Vec<Tag>> {
    Some(match tag {
        Tag::List(l) => l.0.clone(),
        Tag::ByteArray(a) => a.iter().map(|v| Tag::Byte(*v)).collect(),
        Tag::IntArray(a) => a.iter().map(|v| Tag::Int(*v)).collect(),
        Tag::LongArray(a) => a.iter().map(|v| Tag::Long(*v)).collect(),
        _ => return None,
    })
}

fn collection_len(tag: &Tag) -> Option<usize> {
    Some(match tag {
        Tag::List(l) => l.0.len(),
        Tag::ByteArray(a) => a.len(),
        Tag::IntArray(a) => a.len(),
        Tag::LongArray(a) => a.len(),
        _ => return None,
    })
}

fn resolve_index(index: i32, len: usize) -> Option<usize> {
    let len = i64::try_from(len).ok()?;
    let i = if index < 0 {
        len.checked_add(i64::from(index))?
    } else {
        i64::from(index)
    };
    (0..len)
        .contains(&i)
        .then(|| usize::try_from(i).ok())
        .flatten()
}

/// `getTag` of one node.
fn get_node(node: &PathNode, parent: &Tag, out: &mut Vec<Tag>) {
    match node {
        PathNode::Child(name) => {
            if let Tag::Compound(c) = parent
                && let Some(v) = c.get(name)
            {
                out.push(v.clone());
            }
        }
        PathNode::MatchObject(name, pattern) => {
            if let Tag::Compound(c) = parent
                && let Some(v @ Tag::Compound(child)) = c.get(name)
                && compound_matches(pattern, child)
            {
                out.push(v.clone());
            }
        }
        PathNode::AllElements => {
            if let Some(e) = elements(parent) {
                out.extend(e);
            }
        }
        PathNode::Index(index) => {
            if let Some(e) = elements(parent)
                && let Some(i) = resolve_index(*index, e.len())
                && let Some(v) = e.into_iter().nth(i)
            {
                out.push(v);
            }
        }
        PathNode::MatchElement(pattern) => {
            if let Tag::List(l) = parent {
                out.extend(
                    l.0.iter()
                        .filter(|e| matches!(e, Tag::Compound(c) if compound_matches(pattern, c)))
                        .cloned(),
                );
            }
        }
        PathNode::MatchRoot(pattern) => {
            if let Tag::Compound(c) = parent
                && compound_matches(pattern, c)
            {
                out.push(parent.clone());
            }
        }
    }
}

/// The prefix of the path text up to a node (`createNotFoundException`).
fn not_found(path: &NbtPath, node: usize) -> CommandError {
    CommandError::new(
        "arguments.nbtpath.nothing_found",
        vec![ErrArg::Str(prefix(path, node))],
    )
}

/// The path text up to (not including) node `n`: the nodes are re-rendered from the text by
/// counting separators, which is exact for paths written without quotes containing them.
fn prefix(path: &NbtPath, n: usize) -> String {
    // Each node after the first starts at a `.` or `[` outside quotes and braces.
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut starts = vec![0usize];
    let mut prev_bracket_close = false;
    for (i, c) in path.text.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '{' => {
                if depth == 0 && i > 0 && !prev_bracket_close {
                    // A pattern belongs to its node.
                }
                depth = depth.saturating_add(1);
            }
            '}' => depth = depth.saturating_sub(1),
            '[' if depth == 0 => {
                if i > 0 {
                    starts.push(i);
                }
            }
            '.' if depth == 0 => starts.push(i),
            _ => {}
        }
        prev_bracket_close = c == ']';
    }
    let end = starts.get(n).copied().unwrap_or(path.text.len());
    path.text.get(..end).unwrap_or("").to_owned()
}

/// `NbtPath.get`: the matches, or `arguments.nbtpath.nothing_found`.
pub(crate) fn get(path: &NbtPath, root: &Tag) -> Result<Vec<Tag>, CommandError> {
    let mut result = vec![root.clone()];
    for (i, node) in path.nodes.iter().enumerate() {
        let mut next = Vec::new();
        for t in &result {
            get_node(node, t, &mut next);
        }
        if next.is_empty() {
            return Err(not_found(path, i.saturating_add(1)));
        }
        result = next;
    }
    Ok(result)
}

/// `NbtPath.countMatching`.
pub(crate) fn count(path: &NbtPath, root: &Tag) -> usize {
    get(path, root).map_or(0, |r| r.len())
}

/// `createPreferredParentTag` of a node.
fn preferred_parent(node: &PathNode) -> Tag {
    match node {
        PathNode::Child(_) | PathNode::MatchObject(..) | PathNode::MatchRoot(_) => {
            Tag::Compound(Compound::new())
        }
        PathNode::AllElements | PathNode::Index(_) | PathNode::MatchElement(_) => {
            Tag::List(List(Vec::new()))
        }
    }
}

/// Applies `f` to every tag `node` gets or creates (with `create`) in `parent`; returns the
/// sum of `f`.
fn each_or_create(
    node: &PathNode,
    parent: &mut Tag,
    create: &dyn Fn() -> Tag,
    f: &mut dyn FnMut(&mut Tag) -> Result<usize, CommandError>,
) -> Result<usize, CommandError> {
    let mut total = 0usize;
    match node {
        PathNode::Child(name) => {
            if let Tag::Compound(c) = parent {
                if c.get(name).is_none() {
                    c.insert(name.clone(), create());
                }
                if let Some(v) = c.get_mut(name) {
                    total = f(v)?;
                }
            }
        }
        PathNode::MatchObject(name, pattern) => {
            if let Tag::Compound(c) = parent {
                if c.get(name).is_none() {
                    c.insert(name.clone(), Tag::Compound(pattern.clone()));
                }
                if let Some(v) = c.get_mut(name)
                    && matches!(v, Tag::Compound(child) if compound_matches(pattern, child))
                {
                    total = f(v)?;
                }
            }
        }
        PathNode::AllElements => match parent {
            Tag::List(l) => {
                if l.0.is_empty() {
                    l.0.push(create());
                }
                for e in &mut l.0 {
                    total = total.saturating_add(f(e)?);
                }
            }
            other => total = each_numeric(other, f)?,
        },
        PathNode::Index(index) => {
            if let Tag::List(l) = parent
                && let Some(i) = resolve_index(*index, l.0.len())
                && let Some(e) = l.0.get_mut(i)
            {
                total = f(e)?;
            }
        }
        PathNode::MatchElement(pattern) => {
            if let Tag::List(l) = parent {
                let mut found = false;
                for e in &mut l.0 {
                    if matches!(e, Tag::Compound(c) if compound_matches(pattern, c)) {
                        found = true;
                        total = total.saturating_add(f(e)?);
                    }
                }
                if !found {
                    l.0.push(Tag::Compound(pattern.clone()));
                    if let Some(e) = l.0.last_mut() {
                        total = f(e)?;
                    }
                }
            }
        }
        PathNode::MatchRoot(pattern) => {
            if matches!(parent, Tag::Compound(c) if compound_matches(pattern, c)) {
                total = f(parent)?;
            }
        }
    }
    Ok(total)
}

/// Array elements get a copy that is written back.
fn each_numeric(
    parent: &mut Tag,
    f: &mut dyn FnMut(&mut Tag) -> Result<usize, CommandError>,
) -> Result<usize, CommandError> {
    let mut total = 0usize;
    match parent {
        Tag::ByteArray(a) => {
            for v in a.iter_mut() {
                let mut t = Tag::Byte(*v);
                total = total.saturating_add(f(&mut t)?);
                if let Some(n) = t.as_i64() {
                    *v = i8::try_from(n).unwrap_or(*v);
                }
            }
        }
        Tag::IntArray(a) => {
            for v in a.iter_mut() {
                let mut t = Tag::Int(*v);
                total = total.saturating_add(f(&mut t)?);
                if let Some(n) = t.as_i64() {
                    *v = i32::try_from(n).unwrap_or(*v);
                }
            }
        }
        Tag::LongArray(a) => {
            for v in a.iter_mut() {
                let mut t = Tag::Long(*v);
                total = total.saturating_add(f(&mut t)?);
                if let Some(n) = t.as_i64() {
                    *v = n;
                }
            }
        }
        _ => {}
    }
    Ok(total)
}

/// Walks the parents of the last node (creating them like `getOrCreateParents`) and applies
/// `f` to each parent of the last node.
fn parents(
    nodes: &[PathNode],
    tag: &mut Tag,
    f: &mut dyn FnMut(&mut Tag) -> Result<usize, CommandError>,
) -> Result<usize, CommandError> {
    match nodes {
        [] | [_] => f(tag),
        [first, rest @ ..] => {
            let next = rest.first().map(preferred_parent);
            let create = || {
                next.clone()
                    .unwrap_or_else(|| Tag::Compound(Compound::new()))
            };
            each_or_create(first, tag, &create, &mut |child| parents(rest, child, f))
        }
    }
}

/// `NbtPath.isTooDeep`.
pub(crate) fn depth_of(tag: &Tag, depth: usize) -> bool {
    if depth >= MAX_DEPTH {
        return true;
    }
    match tag {
        Tag::Compound(c) => c.iter().any(|(_, v)| depth_of(v, depth.saturating_add(1))),
        Tag::List(l) => l.0.iter().any(|v| depth_of(v, depth.saturating_add(1))),
        _ => false,
    }
}

/// `NbtPath.set`: the number of changed tags.
pub(crate) fn set(path: &NbtPath, root: &mut Tag, value: &Tag) -> Result<usize, CommandError> {
    if depth_of(value, path.nodes.len()) {
        return Err(CommandError::new("arguments.nbtpath.too_deep", vec![]));
    }
    let Some(last) = path.nodes.last() else {
        return Ok(0);
    };
    let mut found_parent = false;
    let changed = parents(&path.nodes, root, &mut |parent| {
        found_parent = true;
        Ok(set_node(last, parent, value))
    })?;
    if !found_parent && path.nodes.len() > 1 {
        return Err(not_found(path, path.nodes.len().saturating_sub(1)));
    }
    Ok(changed)
}

/// `setTag` of the last node.
fn set_node(node: &PathNode, parent: &mut Tag, value: &Tag) -> usize {
    match node {
        PathNode::Child(name) => {
            if let Tag::Compound(c) = parent {
                let old = c.get(name).cloned();
                c.insert(name.clone(), value.clone());
                return usize::from(old.as_ref() != Some(value));
            }
            0
        }
        PathNode::MatchObject(..) | PathNode::MatchRoot(_) => 0,
        PathNode::AllElements => {
            let Some(len) = collection_len(parent) else {
                return 0;
            };
            if len == 0 {
                return usize::from(push_element(parent, value));
            }
            let same = elements(parent)
                .unwrap_or_default()
                .iter()
                .filter(|e| *e == value)
                .count();
            let changed = len.saturating_sub(same);
            if changed == 0 {
                return 0;
            }
            let ok = match parent {
                Tag::List(l) => {
                    l.0 = vec![value.clone(); len];
                    true
                }
                Tag::ByteArray(a) => numeric(value)
                    .and_then(|n| i8::try_from(n).ok())
                    .is_some_and(|n| {
                        a.iter_mut().for_each(|v| *v = n);
                        true
                    }),
                Tag::IntArray(a) => numeric(value)
                    .and_then(|n| i32::try_from(n).ok())
                    .is_some_and(|n| {
                        a.iter_mut().for_each(|v| *v = n);
                        true
                    }),
                Tag::LongArray(a) => numeric(value).is_some_and(|n| {
                    a.iter_mut().for_each(|v| *v = n);
                    true
                }),
                _ => false,
            };
            if ok { changed } else { 0 }
        }
        PathNode::Index(index) => {
            let Some(len) = collection_len(parent) else {
                return 0;
            };
            let Some(i) = resolve_index(*index, len) else {
                return 0;
            };
            let old = elements(parent).and_then(|e| e.into_iter().nth(i));
            if !set_element(parent, i, value) {
                return 0;
            }
            usize::from(old.as_ref() != Some(value))
        }
        PathNode::MatchElement(pattern) => {
            let Tag::List(l) = parent else {
                return 0;
            };
            let mut changed = 0usize;
            for e in &mut l.0 {
                if matches!(e, Tag::Compound(c) if compound_matches(pattern, c)) && e != value {
                    *e = value.clone();
                    changed = changed.saturating_add(1);
                }
            }
            changed
        }
    }
}

fn numeric(tag: &Tag) -> Option<i64> {
    match tag {
        Tag::Byte(_) | Tag::Short(_) | Tag::Int(_) | Tag::Long(_) => tag.as_i64(),
        _ => None,
    }
}

/// `CollectionTag.addTag(size, value)`.
pub(crate) fn push_element(parent: &mut Tag, value: &Tag) -> bool {
    let len = collection_len(parent).unwrap_or(0);
    insert_element(parent, len, value) == Some(true)
}

/// `NumericTag.intValue` and `longValue` (Java's narrowing casts; floating values are floored
/// or truncated and saturate).
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
fn java_numbers(tag: &Tag) -> Option<(i32, i64)> {
    Some(match tag {
        Tag::Byte(_) | Tag::Short(_) | Tag::Int(_) | Tag::Long(_) => {
            let v = tag.as_i64()?;
            (v as i32, v)
        }
        Tag::Float(v) => (v.floor() as i32, *v as i64),
        Tag::Double(v) => (v.floor() as i32, *v as i64),
        _ => return None,
    })
}

/// `CollectionTag.addTag(index, value)`: `None` when `index` is past the end; arrays take
/// numbers (converted like Vanilla's `NumericTag` values), lists any tag.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
pub(crate) fn insert_element(parent: &mut Tag, index: usize, value: &Tag) -> Option<bool> {
    if index > collection_len(parent)? {
        return None;
    }
    let number = java_numbers(value);
    Some(match (parent, number) {
        (Tag::List(l), _) => {
            l.0.insert(index, value.clone());
            true
        }
        (Tag::ByteArray(a), Some((v, _))) => {
            a.insert(index, v as i8);
            true
        }
        (Tag::IntArray(a), Some((v, _))) => {
            a.insert(index, v);
            true
        }
        (Tag::LongArray(a), Some((_, v))) => {
            a.insert(index, v);
            true
        }
        _ => false,
    })
}

/// `CollectionTag.setTag`: arrays take any number (converted like Vanilla's `NumericTag`
/// values), lists any tag.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
fn set_element(parent: &mut Tag, index: usize, value: &Tag) -> bool {
    match (parent, java_numbers(value)) {
        (Tag::List(l), _) => l.0.get_mut(index).is_some_and(|e| {
            *e = value.clone();
            true
        }),
        (Tag::ByteArray(a), Some((v, _))) => a.get_mut(index).is_some_and(|e| {
            *e = v as i8;
            true
        }),
        (Tag::IntArray(a), Some((v, _))) => a.get_mut(index).is_some_and(|e| {
            *e = v;
            true
        }),
        (Tag::LongArray(a), Some((_, v))) => a.get_mut(index).is_some_and(|e| {
            *e = v;
            true
        }),
        _ => false,
    }
}

/// `NbtPath.getOrCreate` with `f` applied to each result; returns the sum of `f`.
pub(crate) fn modify_or_create(
    path: &NbtPath,
    root: &mut Tag,
    create: &dyn Fn() -> Tag,
    f: &mut dyn FnMut(&mut Tag) -> Result<usize, CommandError>,
) -> Result<usize, CommandError> {
    let Some(last) = path.nodes.last() else {
        return f(root);
    };
    let mut any = false;
    let total = parents(&path.nodes, root, &mut |parent| {
        any = true;
        each_or_create(last, parent, create, f)
    })?;
    if !any {
        return Err(not_found(path, path.nodes.len().saturating_sub(1)));
    }
    Ok(total)
}

/// `NbtPath.remove`: the number of removed tags. Parents are not created.
pub(crate) fn remove(path: &NbtPath, root: &mut Tag) -> usize {
    fn walk(nodes: &[PathNode], tag: &mut Tag) -> usize {
        match nodes {
            [] => 0,
            [last] => remove_node(last, tag),
            [first, rest @ ..] => {
                let mut total = 0usize;
                let _ = for_existing(first, tag, &mut |child| {
                    total = total.saturating_add(walk(rest, child));
                });
                total
            }
        }
    }
    walk(&path.nodes, root)
}

/// Applies `f` to the existing tags a node gets (no creation).
fn for_existing(node: &PathNode, parent: &mut Tag, f: &mut dyn FnMut(&mut Tag)) -> Option<()> {
    match node {
        PathNode::Child(name) => f(parent_compound(parent)?.get_mut(name)?),
        PathNode::MatchObject(name, pattern) => {
            let v = parent_compound(parent)?.get_mut(name)?;
            if matches!(v, Tag::Compound(c) if compound_matches(pattern, c)) {
                f(v);
            }
        }
        PathNode::AllElements => {
            if let Tag::List(l) = parent {
                l.0.iter_mut().for_each(f);
            }
        }
        PathNode::Index(index) => {
            if let Tag::List(l) = parent {
                let i = resolve_index(*index, l.0.len())?;
                f(l.0.get_mut(i)?);
            }
        }
        PathNode::MatchElement(pattern) => {
            if let Tag::List(l) = parent {
                for e in &mut l.0 {
                    if matches!(e, Tag::Compound(c) if compound_matches(pattern, c)) {
                        f(e);
                    }
                }
            }
        }
        PathNode::MatchRoot(pattern) => {
            if matches!(parent, Tag::Compound(c) if compound_matches(pattern, c)) {
                f(parent);
            }
        }
    }
    Some(())
}

fn parent_compound(tag: &mut Tag) -> Option<&mut Compound> {
    match tag {
        Tag::Compound(c) => Some(c),
        _ => None,
    }
}

/// `removeTag` of the last node.
fn remove_node(node: &PathNode, parent: &mut Tag) -> usize {
    match node {
        PathNode::Child(name) => parent_compound(parent)
            .and_then(|c| c.remove(name))
            .map_or(0, |_| 1),
        PathNode::MatchObject(name, pattern) => {
            let Some(c) = parent_compound(parent) else {
                return 0;
            };
            if matches!(c.get(name), Some(Tag::Compound(child)) if compound_matches(pattern, child))
            {
                c.remove(name);
                return 1;
            }
            0
        }
        PathNode::AllElements => {
            let len = collection_len(parent).unwrap_or(0);
            match parent {
                Tag::List(l) => l.0.clear(),
                Tag::ByteArray(a) => a.clear(),
                Tag::IntArray(a) => a.clear(),
                Tag::LongArray(a) => a.clear(),
                _ => {}
            }
            len
        }
        PathNode::Index(index) => {
            let Some(len) = collection_len(parent) else {
                return 0;
            };
            let Some(i) = resolve_index(*index, len) else {
                return 0;
            };
            match parent {
                Tag::List(l) => {
                    l.0.remove(i);
                }
                Tag::ByteArray(a) => {
                    a.remove(i);
                }
                Tag::IntArray(a) => {
                    a.remove(i);
                }
                Tag::LongArray(a) => {
                    a.remove(i);
                }
                _ => return 0,
            }
            1
        }
        PathNode::MatchElement(pattern) => {
            let Tag::List(l) = parent else {
                return 0;
            };
            let before = l.0.len();
            l.0.retain(|e| !matches!(e, Tag::Compound(c) if compound_matches(pattern, c)));
            before.saturating_sub(l.0.len())
        }
        PathNode::MatchRoot(_) => 0,
    }
}
