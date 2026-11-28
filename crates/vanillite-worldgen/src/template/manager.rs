//! `StructureTemplateManager`: loads templates of the data packs on first use and shares them.
//! Templates read from files stay in a cache bounded by [`StructureTemplateManager::set_cache_limit`]
//! (least recently used ones go first and are read again when needed); templates put in by
//! structure blocks or created for missing ids stay until removed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use rustc_hash::FxHashMap;
use vanillite_datapack::LoadedPacks;

use super::structure::StructureTemplate;
use crate::Error;
use crate::blocks::qualify;

/// A thread safe cache of structure templates read from the world's
/// `generated/<ns>/structure/<path>.nbt` (when it has one) and then from
/// `data/<ns>/structure/<path>.nbt` of the data packs (later packs override earlier ones, like
/// Vanilla's resource manager).
#[derive(Debug, Default)]
pub struct StructureTemplateManager {
    roots: RwLock<Vec<PathBuf>>,
    /// The world's `generated` directory: read first, structure blocks save there.
    generated: Option<PathBuf>,
    /// Loaded templates; `None` for ids without a file.
    cache: RwLock<FxHashMap<String, Entry>>,
    /// Outlines read by [`Self::outline`]: small (size and jigsaw blocks), so they stay and a
    /// generator that is built again does not read every template file again.
    outlines: RwLock<FxHashMap<String, Arc<StructureTemplate>>>,
    /// Bytes the file-loaded templates of the cache may take.
    cache_limit: AtomicUsize,
    /// Use counter for the least recently used order.
    clock: AtomicU64,
    /// `clock` at the last [`Self::trim_unused`].
    clock_at_trim: AtomicU64,
}

/// Default of [`StructureTemplateManager::set_cache_limit`].
pub const DEFAULT_CACHE_LIMIT: usize = 32 << 20;

#[derive(Debug)]
struct Entry {
    template: Option<Arc<StructureTemplate>>,
    /// Estimated heap size; 0 for entries that cannot be read again from a file.
    bytes: usize,
    /// Whether the entry came from a file and may be evicted.
    reloadable: bool,
    used: AtomicU64,
}

/// A template of a manager by id: read when it is used and again after the cache dropped it, so
/// its holders (structure types, features) keep no template in memory.
#[derive(Clone, Debug)]
pub struct TemplateRef {
    id: Arc<str>,
    manager: Arc<StructureTemplateManager>,
}

impl TemplateRef {
    /// The template `id` of `manager`.
    pub fn new(manager: &Arc<StructureTemplateManager>, id: &str) -> Self {
        Self {
            id: qualify(id).into(),
            manager: Arc::clone(manager),
        }
    }

    /// `getOrCreate(id)`. A file that cannot be read counts as an empty template (Vanilla's
    /// `tryLoad` treats it as missing).
    pub fn get(&self) -> Arc<StructureTemplate> {
        self.manager.get_or_create(&self.id).unwrap_or_default()
    }
}

impl Entry {
    fn fixed(template: Option<Arc<StructureTemplate>>) -> Self {
        Self {
            template,
            bytes: 0,
            reloadable: false,
            used: AtomicU64::new(0),
        }
    }
}

impl StructureTemplateManager {
    /// A manager over the packs' directories.
    pub fn new(packs: &LoadedPacks) -> Self {
        Self::from_roots(packs.roots().to_vec())
    }

    /// A manager over pack directories (each contains `data/`), in load order.
    pub fn from_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            roots: RwLock::new(roots),
            generated: None,
            cache: RwLock::new(FxHashMap::default()),
            outlines: RwLock::new(FxHashMap::default()),
            cache_limit: AtomicUsize::new(DEFAULT_CACHE_LIMIT),
            clock: AtomicU64::new(0),
            clock_at_trim: AtomicU64::new(0),
        }
    }

    /// Bytes the templates read from files may take in the cache; beyond it the least recently
    /// used ones are dropped (and read again when needed).
    pub fn set_cache_limit(&self, bytes: usize) {
        self.cache_limit.store(bytes, Ordering::Relaxed);
        let mut cache = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        Self::evict(&mut cache, bytes);
    }

    /// Drops every template that can be read again from its file (an idle server keeps none).
    pub fn trim(&self) {
        let mut cache = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        cache.retain(|_, e| !e.reloadable);
        cache.shrink_to_fit();
    }

    /// [`Self::trim`] if no template was asked for since the last call (called every 30 s:
    /// world generation placed no structure meanwhile).
    pub fn trim_unused(&self) {
        let now = self.clock.load(Ordering::Relaxed);
        if self.clock_at_trim.swap(now, Ordering::Relaxed) == now {
            self.trim();
        }
    }

    /// Drops least recently used file-loaded templates until they take at most `limit` bytes.
    fn evict(cache: &mut FxHashMap<String, Entry>, limit: usize) {
        let mut total: usize = cache.values().map(|e| e.bytes).sum();
        if total <= limit {
            return;
        }
        let mut order: Vec<(u64, &String)> = cache
            .iter()
            .filter(|(_, e)| e.reloadable && e.bytes > 0)
            .map(|(id, e)| (e.used.load(Ordering::Relaxed), id))
            .collect();
        order.sort_unstable();
        let mut gone = Vec::new();
        for (_, id) in order {
            if total <= limit {
                break;
            }
            total -= cache[id].bytes;
            gone.push(id.clone());
        }
        for id in gone {
            cache.remove(&id);
        }
    }

    /// The template of `id` for reading its outline (size and jigsaw blocks): the cached one,
    /// or one read from the files with only its jigsaw blocks, kept as outline. Missing ids
    /// behave as in [`Self::get_or_create`].
    pub fn outline(&self, id: &str) -> Result<Arc<StructureTemplate>, Error> {
        let id = qualify(id);
        if let Some(entry) = self
            .cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            && let Some(template) = &entry.template
        {
            return Ok(Arc::clone(template));
        }
        if let Some(outline) = self
            .outlines
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
        {
            return Ok(Arc::clone(outline));
        }
        match self.load(&id, true)? {
            Some(template) => Ok(Arc::clone(
                self.outlines
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(id)
                    .or_insert(template),
            )),
            None => self.get_or_create(&id),
        }
    }

    /// A manager of a world: its `generated` directory first, then the pack directories.
    pub fn for_world(roots: Vec<PathBuf>, generated: PathBuf) -> Self {
        Self {
            generated: Some(generated),
            ..Self::from_roots(roots)
        }
    }

    /// `onResourceManagerReload`: new pack directories, every cached template goes.
    pub fn reload(&self, roots: Vec<PathBuf>) {
        *self.roots.write().unwrap_or_else(PoisonError::into_inner) = roots;
        self.cache
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.outlines
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// The file of `id` below a `generated` directory (`<ns>/structure/<path>.nbt`).
    fn generated_file(dir: &Path, id: &str) -> Option<PathBuf> {
        let (namespace, path) = id.split_once(':')?;
        if !portable(namespace) || !path.split('/').all(portable) {
            return None;
        }
        Some(
            dir.join(namespace)
                .join("structure")
                .join(format!("{path}.nbt")),
        )
    }

    /// Puts `template` into the cache as `id` (`getOrCreate(id)` filled in place).
    pub fn put(&self, id: &str, template: StructureTemplate) {
        let id = qualify(id);
        self.forget_outline(&id);
        self.cache
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, Entry::fixed(Some(Arc::new(template))));
    }

    /// `remove(id)`: drops the cached template, the next `get` reads the files again.
    pub fn remove(&self, id: &str) {
        let id = qualify(id);
        self.forget_outline(&id);
        self.cache
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// The files of `id` may have changed: its outline is read again.
    fn forget_outline(&self, id: &str) {
        self.outlines
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }

    /// `save(id)`: writes the cached template of `id` to the world's `generated` directory.
    /// `Ok(false)` without a cached template, a `generated` directory or a valid file name; an
    /// error when writing fails.
    pub fn save(&self, id: &str) -> Result<bool, Error> {
        let id = qualify(id);
        let Some(template) = self
            .cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .and_then(|e| e.template.clone())
        else {
            return Ok(false);
        };
        let Some(file) = self
            .generated
            .as_deref()
            .and_then(|dir| Self::generated_file(dir, &id))
        else {
            return Ok(false);
        };
        let failed = |e: &dyn std::fmt::Display| {
            Error::new(format!("structure file {id} to {}: {e}", file.display()))
        };
        let bytes = template.to_gzip().map_err(|e| failed(&e))?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| failed(&e))?;
        }
        std::fs::write(&file, bytes).map_err(|e| failed(&e))?;
        self.forget_outline(&id);
        Ok(true)
    }

    fn file(root: &Path, id: &str) -> Option<PathBuf> {
        let (namespace, path) = id.split_once(':')?;
        if path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        {
            return None;
        }
        Some(
            root.join("data")
                .join(namespace)
                .join("structure")
                .join(format!("{path}.nbt")),
        )
    }

    /// Reads `id` from the files; `outline` keeps only the jigsaw blocks of its palettes.
    fn load(&self, id: &str, outline: bool) -> Result<Option<Arc<StructureTemplate>>, Error> {
        let parse = |bytes: &[u8]| {
            if outline {
                StructureTemplate::outline_from_gzip(bytes)
            } else {
                StructureTemplate::from_gzip(bytes)
            }
        };
        if let Some(file) = self
            .generated
            .as_deref()
            .and_then(|dir| Self::generated_file(dir, id))
        {
            match std::fs::read(&file) {
                Ok(bytes) => {
                    // `tryLoad`: a source that fails counts as not having the template.
                    if let Ok(template) = parse(&bytes) {
                        return Ok(Some(Arc::new(template)));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::new(format!("{}: {e}", file.display()))),
            }
        }
        let roots = self
            .roots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for root in roots.iter().rev() {
            let Some(file) = Self::file(root, id) else {
                return Ok(None);
            };
            match std::fs::read(&file) {
                Ok(bytes) => {
                    let template =
                        parse(&bytes).map_err(|e| Error::new(format!("structure {id}: {e}")))?;
                    return Ok(Some(Arc::new(template)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::new(format!("{}: {e}", file.display()))),
            }
        }
        Ok(None)
    }

    /// `get(id)`: the template, loaded on first use; `None` if no pack has it. A file that
    /// cannot be read is an error (Vanilla logs it and treats the template as missing).
    pub fn get(&self, id: &str) -> Result<Option<Arc<StructureTemplate>>, Error> {
        let id = qualify(id);
        let now = self.clock.fetch_add(1, Ordering::Relaxed);
        if let Some(entry) = self
            .cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
        {
            entry.used.store(now, Ordering::Relaxed);
            return Ok(entry.template.clone());
        }
        let loaded = self.load(&id, false)?;
        let bytes = loaded.as_ref().map_or(0, |t| t.heap_size());
        let mut cache = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        let template = cache
            .entry(id)
            .or_insert(Entry {
                template: loaded,
                bytes,
                reloadable: true,
                used: AtomicU64::new(now),
            })
            .template
            .clone();
        Self::evict(&mut cache, self.cache_limit.load(Ordering::Relaxed));
        Ok(template)
    }

    /// `getOrCreate(id)`: like [`Self::get`], with an empty template for missing ids (placing it
    /// does nothing).
    pub fn get_or_create(&self, id: &str) -> Result<Arc<StructureTemplate>, Error> {
        let id = qualify(id);
        if let Some(template) = self.get(&id)? {
            return Ok(template);
        }
        let mut cache = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        let entry = cache.entry(id).or_insert_with(|| Entry::fixed(None));
        if entry.template.is_none() {
            // An empty template that is not backed by a file must stay.
            *entry = Entry::fixed(Some(Arc::new(StructureTemplate::default())));
        }
        Ok(entry.template.clone().unwrap_or_default())
    }

    /// `listTemplates()`: the ids of every template file of the packs, sorted.
    pub fn list_templates(&self) -> Result<Vec<String>, Error> {
        let mut ids = Vec::new();
        if let Some(dir) = &self.generated
            && let Ok(namespaces) = std::fs::read_dir(dir)
        {
            for namespace in namespaces.flatten() {
                let Some(ns) = namespace.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let base = namespace.path().join("structure");
                if base.is_dir() {
                    collect(&base, &base, &ns, &mut ids)?;
                }
            }
        }
        let roots = self
            .roots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for root in &roots {
            let data = root.join("data");
            let Ok(namespaces) = std::fs::read_dir(&data) else {
                continue;
            };
            for namespace in namespaces {
                let namespace =
                    namespace.map_err(|e| Error::new(format!("{}: {e}", data.display())))?;
                let Some(ns) = namespace.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let base = namespace.path().join("structure");
                if base.is_dir() {
                    collect(&base, &base, &ns, &mut ids)?;
                }
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

/// `FileUtil.isPathPartPortable` and the identifier rules a path part must meet.
fn portable(part: &str) -> bool {
    !part.is_empty()
        && !part.ends_with('.')
        && part
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
        && !RESERVED.iter().any(|r| {
            part.split('.')
                .next()
                .is_some_and(|stem| stem.eq_ignore_ascii_case(r))
        })
}

/// Windows' reserved file names (`FileUtil.RESERVED_WINDOWS_FILENAMES`).
const RESERVED: [&str; 24] = [
    "com", "clock$", "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6",
    "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn collect(base: &Path, dir: &Path, ns: &str, out: &mut Vec<String>) -> Result<(), Error> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| Error::new(format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let path = entry
            .map_err(|e| Error::new(format!("{}: {e}", dir.display())))?
            .path();
        if path.is_dir() {
            collect(base, &path, ns, out)?;
        } else if path.extension().is_some_and(|e| e == "nbt")
            && let Ok(rel) = path.with_extension("").strip_prefix(base)
        {
            let parts: Vec<&str> = rel.iter().filter_map(|p| p.to_str()).collect();
            out.push(format!("{ns}:{}", parts.join("/")));
        }
    }
    Ok(())
}
