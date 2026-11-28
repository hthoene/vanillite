//! Recipes (`RecipeManager`): loaded from `data/<ns>/recipe/**.json` of the data packs, matched
//! like Vanilla.
//!
//! Recipes of one type are tried in the order of their ids (`RecipeManager` keeps a sorted
//! map; `Identifier` sorts by path, then namespace); the first one that matches wins.
//!
//! - crafting: `crafting_shaped` (with the mirrored pattern), `crafting_shapeless`,
//!   `crafting_transmute`, `crafting_imbue`, `crafting_dye`, `crafting_decorated_pot`,
//!   `crafting_special_repairitem`, `crafting_special_bookcloning`,
//!   `crafting_special_bannerduplicate`, `crafting_special_shielddecoration`,
//!   `crafting_special_mapextending` (with the level's maps, [`Recipes::crafting_for_in`]),
//!   `crafting_special_firework_star`, `crafting_special_firework_star_fade` and
//!   `crafting_special_firework_rocket`.
//! - cooking: `smelting`, `blasting`, `smoking`, `campfire_cooking`.
//! - `stonecutting`, `smithing_transform`, `smithing_trim`.
//!
//! Furnace fuels come from the items' `minecraft:cooking_fuel` burn time providers
//! (`data/<ns>/context_int_provider/`), evaluated per furnace kind.

pub mod book;
mod fuel;
pub mod place;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde_json::Value;
use vanillite_data::registry::{ITEM, MOB_EFFECT, POTION};
use vanillite_protocol::codec::write_varint;
use vanillite_protocol::components::ids as components;

use crate::item::{self, ItemStack};

/// A set of items (`Ingredient`): sorted item ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ingredient(Box<[u16]>);

impl Ingredient {
    /// `Ingredient.test`.
    #[inline]
    pub fn test(&self, stack: &ItemStack) -> bool {
        !stack.is_empty() && self.0.binary_search(&stack.item).is_ok()
    }

    /// The items.
    pub fn items(&self) -> &[u16] {
        &self.0
    }
}

/// `TransmuteResult`: the item to turn the input into (`None` keeps the input's item), the
/// count and the result's own components.
#[derive(Clone, Debug)]
pub struct TransmuteResult {
    item: Option<u16>,
    count: i32,
}

/// A crafting recipe kind.
#[derive(Clone, Debug)]
pub enum Crafting {
    /// `crafting_shaped`.
    Shaped {
        width: usize,
        height: usize,
        /// Index into `ingredients` per cell, `None` for an empty cell.
        cells: Box<[Option<usize>]>,
        ingredients: Vec<Ingredient>,
        symmetrical: bool,
        result: ItemStack,
    },
    /// `crafting_shapeless`.
    Shapeless {
        ingredients: Vec<Ingredient>,
        result: ItemStack,
    },
    /// `crafting_transmute`.
    Transmute {
        input: Ingredient,
        material: Ingredient,
        min: i32,
        max: i32,
        add_material_count: bool,
        result: TransmuteResult,
    },
    /// `crafting_imbue` (tipped arrows).
    Imbue {
        source: Ingredient,
        material: Ingredient,
        result: ItemStack,
    },
    /// `crafting_decorated_pot`.
    DecoratedPot {
        back: Ingredient,
        left: Ingredient,
        right: Ingredient,
        front: Ingredient,
        result: ItemStack,
    },
    /// `crafting_special_repairitem`.
    RepairItem,
    /// `crafting_special_bookcloning`.
    BookCloning {
        source: Ingredient,
        material: Ingredient,
        result: ItemStack,
    },
    /// `crafting_dye`.
    Dye {
        target: Ingredient,
        dye: Ingredient,
        result: TransmuteResult,
    },
    /// `crafting_special_bannerduplicate`.
    BannerDuplicate {
        banner: Ingredient,
        result: TransmuteResult,
    },
    /// `crafting_special_shielddecoration`.
    ShieldDecoration {
        banner: Ingredient,
        target: Ingredient,
        result: TransmuteResult,
    },
    /// `crafting_special_mapextending`.
    MapExtending {
        map: Ingredient,
        material: Ingredient,
        result: TransmuteResult,
    },
    /// `crafting_special_firework_star`: shapes as `(FireworkExplosion.Shape id, ingredient)`.
    FireworkStar {
        shapes: Vec<(i32, Ingredient)>,
        trail: Ingredient,
        twinkle: Ingredient,
        fuel: Ingredient,
        dye: Ingredient,
        result: ItemStack,
    },
    /// `crafting_special_firework_star_fade`.
    FireworkStarFade {
        target: Ingredient,
        dye: Ingredient,
        result: ItemStack,
    },
    /// `crafting_special_firework_rocket`.
    FireworkRocket {
        shell: Ingredient,
        fuel: Ingredient,
        star: Ingredient,
        result: ItemStack,
    },
}

/// A crafting recipe with its id.
#[derive(Clone, Debug)]
pub struct CraftingRecipe {
    /// Recipe id.
    pub id: String,
    /// The kind.
    pub kind: Crafting,
}

/// Cooking recipe types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CookingKind {
    /// `smelting` (furnace).
    Smelting,
    /// `blasting` (blast furnace).
    Blasting,
    /// `smoking` (smoker).
    Smoking,
    /// `campfire_cooking`.
    Campfire,
}

/// A cooking recipe (`AbstractCookingRecipe`).
#[derive(Clone, Debug)]
pub struct Cooking {
    /// Recipe id.
    pub id: String,
    /// Type.
    pub kind: CookingKind,
    /// Input.
    pub ingredient: Ingredient,
    /// Result.
    pub result: ItemStack,
    /// Experience per item.
    pub experience: f32,
    /// Ticks to cook one item.
    pub cooking_time: i32,
}

/// A stonecutter recipe.
#[derive(Clone, Debug)]
pub struct Stonecutting {
    /// Recipe id.
    pub id: String,
    /// Input.
    pub ingredient: Ingredient,
    /// Result.
    pub result: ItemStack,
}

/// A smithing recipe.
#[derive(Clone, Debug)]
pub enum Smithing {
    /// `smithing_transform`: the base becomes the result with its components.
    Transform {
        template: Option<Ingredient>,
        base: Option<Ingredient>,
        addition: Option<Ingredient>,
        result: ItemStack,
    },
    /// `smithing_trim`: the base gets the `trim` of the addition's material and the pattern.
    Trim {
        template: Ingredient,
        base: Ingredient,
        addition: Ingredient,
        /// `minecraft:trim_pattern` id.
        pattern: String,
    },
}

/// A brewing recipe (`brewing`, data driven in 26.3): a reagent turns a bottle of one potion
/// into another.
#[derive(Clone, Debug)]
pub struct Brewing {
    /// Recipe id.
    pub id: String,
    /// The bottle item.
    pub input: Ingredient,
    /// The bottle's potion (`minecraft:potion` id), if the recipe requires one.
    pub input_potion: Option<u16>,
    /// The reagent.
    pub reagent: Ingredient,
    /// The result (with its `potion_contents`).
    pub output: ItemStack,
}

/// The potion of a stack's `potion_contents` (`PotionContents.potion`).
pub fn potion_of(stack: &ItemStack) -> Option<u16> {
    let data = stack.component(components::POTION_CONTENTS)?;
    let mut r = vanillite_protocol::codec::Reader::new(data);
    if r.take(1).ok()?.first().copied()? == 0 {
        return None;
    }
    u16::try_from(r.varint().ok()?).ok()
}

/// A `potion_contents` value with only a potion.
pub(crate) fn potion_contents(potion: u16) -> Box<[u8]> {
    let mut out = vec![1];
    write_varint(&mut out, i32::from(potion));
    // No custom colour, no custom effects, no custom name.
    out.extend_from_slice(&[0, 0, 0]);
    out.into_boxed_slice()
}

/// What a crafting grid holds (`CraftingInput.Positioned`): the trimmed grid and its offset in
/// the full grid.
#[derive(Clone, Debug, Default)]
pub struct CraftingInput {
    /// Width of the trimmed grid.
    pub width: usize,
    /// Height of the trimmed grid.
    pub height: usize,
    /// Stacks, row by row.
    pub items: Vec<ItemStack>,
    /// Column of the trimmed grid in the full grid.
    pub left: usize,
    /// Row of the trimmed grid in the full grid.
    pub top: usize,
    ingredient_count: usize,
}

impl CraftingInput {
    /// `CraftingInput.ofPositioned(width, height, items)`.
    pub fn of(width: usize, height: usize, grid: &[ItemStack]) -> CraftingInput {
        let mut left = width;
        let mut right = 0;
        let mut top = height;
        let mut bottom = 0;
        let mut any = false;
        for y in 0..height {
            for x in 0..width {
                if grid.get(x + y * width).is_some_and(|s| !s.is_empty()) {
                    any = true;
                    left = left.min(x);
                    right = right.max(x);
                    top = top.min(y);
                    bottom = bottom.max(y);
                }
            }
        }
        if !any {
            return CraftingInput::default();
        }
        let w = right - left + 1;
        let h = bottom - top + 1;
        let mut items = Vec::with_capacity(w * h);
        for y in top..=bottom {
            for x in left..=right {
                items.push(grid.get(x + y * width).cloned().unwrap_or_default());
            }
        }
        let ingredient_count = items.iter().filter(|s| !s.is_empty()).count();
        CraftingInput {
            width: w,
            height: h,
            items,
            left,
            top,
            ingredient_count,
        }
    }

    /// `getItem(x, y)`.
    pub fn get(&self, x: usize, y: usize) -> &ItemStack {
        self.items
            .get(x + y * self.width)
            .unwrap_or(&ItemStack::EMPTY)
    }

    /// Number of non-empty stacks.
    pub fn ingredient_count(&self) -> usize {
        self.ingredient_count
    }

    fn non_empty(&self) -> impl Iterator<Item = &ItemStack> {
        self.items.iter().filter(|s| !s.is_empty())
    }
}

/// Burn time and speed of a fuel in one furnace kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fuel {
    /// Burn time in ticks.
    pub burn_time: i32,
    /// `speed_multiplier`.
    pub speed: f32,
}

/// All recipes of the loaded data packs.
#[derive(Debug, Default)]
pub struct Recipes {
    /// Crafting recipes in id order.
    pub crafting: Vec<CraftingRecipe>,
    /// Cooking recipes in id order.
    pub cooking: Vec<Cooking>,
    /// Stonecutter recipes in id order.
    pub stonecutting: Vec<Stonecutting>,
    /// Smithing recipes in id order.
    pub smithing: Vec<(String, Smithing)>,
    /// Brewing recipes in id order.
    pub brewing: Vec<Brewing>,
    /// Recipes that could not be loaded, with the reason.
    pub broken: Vec<String>,
    /// The recipe book displays.
    pub book: book::Book,
    /// The `smithing_template`, `smithing_base` and `smithing_addition` property sets.
    smithing_sets: (Vec<u16>, Vec<u16>, Vec<u16>),
    /// Fuels of normal furnaces by item id.
    fuel_normal: HashMap<u16, Fuel>,
    /// Fuels of blast furnaces and smokers (`fast_cooking`) by item id.
    fuel_fast: HashMap<u16, Fuel>,
    /// The `context_int_provider` and `context_float_provider` entries (patched
    /// `cooking_fuel` values refer to them).
    providers: (HashMap<String, Value>, HashMap<String, Value>),
}

/// A `ResolvableInt` / `ResolvableFloat` from the wire (`either(INT or FLOAT, ResourceKey)`)
/// as the provider JSON it stands for: the number or the provider id.
pub fn read_resolvable(
    r: &mut vanillite_protocol::codec::Reader<'_>,
    float: bool,
) -> Option<Value> {
    if r.u8().ok()? != 0 {
        if float {
            serde_json::Number::from_f64(f64::from(r.f32().ok()?)).map(Value::Number)
        } else {
            Some(Value::from(r.i32().ok()?))
        }
    } else {
        Some(Value::String(r.string(32767).ok()?.to_owned()))
    }
}

/// Items whose crafting leaves something behind (`Item.Properties.craftRemainder`).
pub fn crafting_remainder(item: u16) -> Option<u16> {
    use vanillite_data::item::ids;
    let i = vanillite_data::item::ItemId(item);
    if i == ids::WATER_BUCKET || i == ids::LAVA_BUCKET || i == ids::MILK_BUCKET {
        Some(ids::BUCKET.0)
    } else if i == ids::DRAGON_BREATH || i == ids::HONEY_BOTTLE {
        Some(ids::GLASS_BOTTLE.0)
    } else {
        None
    }
}

/// `CraftingRecipe.defaultCraftingReminder`: the crafting remainder of every input stack.
pub fn default_remainders(input: &CraftingInput) -> Vec<ItemStack> {
    input
        .items
        .iter()
        .map(|s| {
            if s.is_empty() {
                return ItemStack::EMPTY;
            }
            crafting_remainder(s.item).map_or(ItemStack::EMPTY, |r| ItemStack::new(r, 1))
        })
        .collect()
}

impl Recipes {
    /// Loads every recipe of the packs in `roots` (later packs override earlier ones).
    /// `tag` resolves an item tag id (without `#`) to item ids.
    pub fn load(roots: &[PathBuf], tag: &dyn Fn(&str) -> Vec<u16>) -> Recipes {
        let mut recipe_files = BTreeMap::new();
        let mut int_files = BTreeMap::new();
        let mut float_files = BTreeMap::new();
        for root in roots {
            let Ok(dir) = std::fs::read_dir(root.join("data")) else {
                continue;
            };
            let mut namespaces: Vec<(String, PathBuf)> = dir
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
                .collect();
            namespaces.sort();
            for (ns, path) in namespaces {
                crate::loot::collect_json(
                    &path.join("recipe"),
                    &format!("{ns}:"),
                    &mut recipe_files,
                );
                crate::loot::collect_json(
                    &path.join("context_int_provider"),
                    &format!("{ns}:"),
                    &mut int_files,
                );
                crate::loot::collect_json(
                    &path.join("context_float_provider"),
                    &format!("{ns}:"),
                    &mut float_files,
                );
            }
        }
        let mut parsed: Vec<(String, Value)> = crate::loot::read_files(&recipe_files)
            .into_iter()
            .filter_map(|(id, r)| match r {
                Ok(v) => Some((id, v)),
                Err(e) => {
                    log::warn!("recipe {id}: {e}");
                    None
                }
            })
            .collect();
        // `Identifier.compareTo`: path first, then namespace.
        parsed.sort_by(|(a, _), (b, _)| {
            let split = |s: &str| {
                let (ns, path) = s.split_once(':').unwrap_or(("minecraft", s));
                (path.to_owned(), ns.to_owned())
            };
            split(a).cmp(&split(b))
        });
        let mut recipes = Recipes::default();
        let mut unsupported = 0usize;
        let mut info = HashMap::new();
        for (id, json) in parsed {
            if let Err(e) = recipes.add(&id, &json, tag) {
                unsupported += 1;
                log::debug!("recipe {id}: {e}");
                recipes.broken.push(format!("{id}: {e}"));
            } else {
                info.insert(id, book::RecipeInfo::of(&json));
            }
        }
        recipes.book = book::Book::build(&recipes, &info);
        if unsupported > 0 {
            log::warn!("{unsupported} recipes could not be loaded");
        }
        let ints: HashMap<String, Value> = crate::loot::read_files(&int_files)
            .into_iter()
            .filter_map(|(id, r)| Some((id, r.ok()?)))
            .collect();
        let floats: HashMap<String, Value> = crate::loot::read_files(&float_files)
            .into_iter()
            .filter_map(|(id, r)| Some((id, r.ok()?)))
            .collect();
        let mut sets = (Vec::new(), Vec::new(), Vec::new());
        for (_, r) in &recipes.smithing {
            let (t, b, a) = match r {
                Smithing::Transform {
                    template,
                    base,
                    addition,
                    ..
                } => (template.as_ref(), base.as_ref(), addition.as_ref()),
                Smithing::Trim {
                    template,
                    base,
                    addition,
                    ..
                } => (Some(template), Some(base), Some(addition)),
            };
            sets.0
                .extend(t.map(|i| i.items().to_vec()).unwrap_or_default());
            sets.1
                .extend(b.map(|i| i.items().to_vec()).unwrap_or_default());
            sets.2
                .extend(a.map(|i| i.items().to_vec()).unwrap_or_default());
        }
        for set in [&mut sets.0, &mut sets.1, &mut sets.2] {
            set.sort_unstable();
            set.dedup();
        }
        recipes.smithing_sets = sets;
        for (provider, items) in fuel::FUELS {
            for fast in [false, true] {
                let ctx = ProviderContext {
                    ints: &ints,
                    floats: &floats,
                    fast,
                };
                let burn_time = ctx.int(&Value::String((*provider).to_owned()), 0);
                let speed = ctx.float(
                    &Value::String("minecraft:cooking/speed_default".to_owned()),
                    0,
                );
                for name in *items {
                    if let Some(item) = ITEM.id(&format!("minecraft:{name}")) {
                        let table = if fast {
                            &mut recipes.fuel_fast
                        } else {
                            &mut recipes.fuel_normal
                        };
                        table.insert(item, Fuel { burn_time, speed });
                    }
                }
            }
        }
        recipes.providers = (ints, floats);
        recipes
    }

    fn add(
        &mut self,
        id: &str,
        json: &Value,
        tag: &dyn Fn(&str) -> Vec<u16>,
    ) -> Result<(), String> {
        let kind = json
            .get("type")
            .and_then(Value::as_str)
            .ok_or("no type")?
            .trim_start_matches("minecraft:");
        let ing = |key: &str| -> Result<Ingredient, String> {
            ingredient(json.get(key).ok_or_else(|| format!("no {key}"))?, tag)
        };
        let opt_ing = |key: &str| -> Result<Option<Ingredient>, String> {
            json.get(key).map(|v| ingredient(v, tag)).transpose()
        };
        let res = || result(json.get("result").ok_or("no result")?);
        let crafting = |kind: Crafting| CraftingRecipe {
            id: id.to_owned(),
            kind,
        };
        match kind {
            "crafting_shaped" => {
                let recipe = shaped(json, tag)?;
                self.crafting.push(crafting(recipe));
            }
            "crafting_shapeless" => {
                let ingredients = json
                    .get("ingredients")
                    .and_then(Value::as_array)
                    .ok_or("no ingredients")?
                    .iter()
                    .map(|v| ingredient(v, tag))
                    .collect::<Result<Vec<_>, _>>()?;
                self.crafting.push(crafting(Crafting::Shapeless {
                    ingredients,
                    result: res()?,
                }));
            }
            "crafting_transmute" => {
                let (min, max) = match json.get("material_count") {
                    None => (1, 1),
                    Some(Value::Number(n)) => {
                        let n = n.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(1);
                        (n, n)
                    }
                    Some(v) => (
                        v.get("min")
                            .and_then(Value::as_i64)
                            .and_then(|n| i32::try_from(n).ok())
                            .unwrap_or(1),
                        v.get("max")
                            .and_then(Value::as_i64)
                            .and_then(|n| i32::try_from(n).ok())
                            .unwrap_or(8),
                    ),
                };
                let result = json.get("result").ok_or("no result")?;
                let result = match result {
                    Value::String(s) => TransmuteResult {
                        item: Some(item_id(s)?),
                        count: 1,
                    },
                    v => TransmuteResult {
                        item: v
                            .get("id")
                            .and_then(Value::as_str)
                            .map(item_id)
                            .transpose()?,
                        count: count_of(v),
                    },
                };
                self.crafting.push(crafting(Crafting::Transmute {
                    input: ing("input")?,
                    material: ing("material")?,
                    min,
                    max,
                    add_material_count: json
                        .get("add_material_count_to_result")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    result,
                }));
            }
            "crafting_imbue" => self.crafting.push(crafting(Crafting::Imbue {
                source: ing("source")?,
                material: ing("material")?,
                result: res()?,
            })),
            "crafting_decorated_pot" => self.crafting.push(crafting(Crafting::DecoratedPot {
                back: ing("back")?,
                left: ing("left")?,
                right: ing("right")?,
                front: ing("front")?,
                result: res()?,
            })),
            "crafting_special_repairitem" => self.crafting.push(crafting(Crafting::RepairItem)),
            "crafting_special_bookcloning" => self.crafting.push(crafting(Crafting::BookCloning {
                source: ing("source")?,
                material: ing("material")?,
                result: res()?,
            })),
            "crafting_dye" => self.crafting.push(crafting(Crafting::Dye {
                target: ing("target")?,
                dye: ing("dye")?,
                result: transmute_result(json)?,
            })),
            "crafting_special_bannerduplicate" => {
                self.crafting.push(crafting(Crafting::BannerDuplicate {
                    banner: ing("banner")?,
                    result: transmute_result(json)?,
                }));
            }
            "crafting_special_shielddecoration" => {
                self.crafting.push(crafting(Crafting::ShieldDecoration {
                    banner: ing("banner")?,
                    target: ing("target")?,
                    result: transmute_result(json)?,
                }));
            }
            "crafting_special_mapextending" => {
                self.crafting.push(crafting(Crafting::MapExtending {
                    map: ing("map")?,
                    material: ing("material")?,
                    result: transmute_result(json)?,
                }));
            }
            "crafting_special_firework_star" => {
                let mut shapes = Vec::new();
                let map = json
                    .get("shapes")
                    .and_then(Value::as_object)
                    .ok_or("no shapes")?;
                for (name, value) in map {
                    let shape = FIREWORK_SHAPES
                        .iter()
                        .position(|s| s == name)
                        .and_then(|i| i32::try_from(i).ok())
                        .ok_or_else(|| format!("unknown firework shape {name}"))?;
                    shapes.push((shape, ingredient(value, tag)?));
                }
                self.crafting.push(crafting(Crafting::FireworkStar {
                    shapes,
                    trail: ing("trail")?,
                    twinkle: ing("twinkle")?,
                    fuel: ing("fuel")?,
                    dye: ing("dye")?,
                    result: res()?,
                }));
            }
            "crafting_special_firework_star_fade" => {
                self.crafting.push(crafting(Crafting::FireworkStarFade {
                    target: ing("target")?,
                    dye: ing("dye")?,
                    result: res()?,
                }));
            }
            "crafting_special_firework_rocket" => {
                self.crafting.push(crafting(Crafting::FireworkRocket {
                    shell: ing("shell")?,
                    fuel: ing("fuel")?,
                    star: ing("star")?,
                    result: res()?,
                }));
            }
            "smelting" | "blasting" | "smoking" | "campfire_cooking" => {
                let kind = match kind {
                    "smelting" => CookingKind::Smelting,
                    "blasting" => CookingKind::Blasting,
                    "smoking" => CookingKind::Smoking,
                    _ => CookingKind::Campfire,
                };
                let default_time = match kind {
                    CookingKind::Smelting => 200,
                    CookingKind::Blasting | CookingKind::Smoking => 100,
                    CookingKind::Campfire => 100,
                };
                #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
                let experience = json
                    .get("experience")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0) as f32;
                self.cooking.push(Cooking {
                    id: id.to_owned(),
                    kind,
                    ingredient: ing("ingredient")?,
                    result: res()?,
                    experience,
                    cooking_time: json
                        .get("cookingtime")
                        .and_then(Value::as_i64)
                        .and_then(|n| i32::try_from(n).ok())
                        .unwrap_or(default_time),
                });
            }
            "stonecutting" => self.stonecutting.push(Stonecutting {
                id: id.to_owned(),
                ingredient: ing("ingredient")?,
                result: res()?,
            }),
            "smithing_transform" => self.smithing.push((
                id.to_owned(),
                Smithing::Transform {
                    template: opt_ing("template")?,
                    base: opt_ing("base")?,
                    addition: opt_ing("addition")?,
                    result: res()?,
                },
            )),
            "smithing_trim" => self.smithing.push((
                id.to_owned(),
                Smithing::Trim {
                    template: ing("template")?,
                    base: ing("base")?,
                    addition: ing("addition")?,
                    pattern: json
                        .get("pattern")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                },
            )),
            "brewing" => {
                let input = json.get("input").ok_or("no input")?;
                let input_potion = input
                    .get("potion_contents")
                    .and_then(|p| p.get("potions"))
                    .and_then(Value::as_str)
                    .map(|p| POTION.id(p).ok_or_else(|| format!("unknown potion {p}")))
                    .transpose()?;
                let output = json.get("output").ok_or("no output")?;
                let mut out = ItemStack::new(
                    item_id(
                        output
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or("no output id")?,
                    )?,
                    count_of(output),
                );
                if let Some(potion) = output
                    .get("components")
                    .and_then(|c| c.get("minecraft:potion_contents"))
                    .and_then(|p| p.get("potion"))
                    .and_then(Value::as_str)
                {
                    let id = POTION
                        .id(potion)
                        .ok_or_else(|| format!("unknown potion {potion}"))?;
                    out.set_component(components::POTION_CONTENTS, potion_contents(id));
                }
                self.brewing.push(Brewing {
                    id: id.to_owned(),
                    input: ingredient(input.get("item").ok_or("no input item")?, tag)?,
                    input_potion,
                    reagent: ingredient(
                        json.get("reagent")
                            .and_then(|r| r.get("item"))
                            .ok_or("no reagent")?,
                        tag,
                    )?,
                    output: out,
                });
            }
            other => return Err(format!("unknown recipe type {other}")),
        }
        Ok(())
    }

    /// `Recipe.placementInfo()` with the shape (`None` for `NOT_PLACEABLE`).
    pub fn placement(&self, r: book::RecipeRef) -> Option<place::Placement> {
        let seq = |ingredients: Vec<Ingredient>| place::Placement {
            slots: (0..ingredients.len()).map(Some).collect(),
            ingredients,
            shaped: None,
        };
        match r {
            book::RecipeRef::Crafting(i) => match &self.crafting.get(i)?.kind {
                Crafting::Shaped {
                    width,
                    height,
                    cells,
                    ingredients,
                    ..
                } => {
                    let mut out = Vec::new();
                    let mut slots = Vec::with_capacity(cells.len());
                    for cell in cells.iter() {
                        match cell.and_then(|c| ingredients.get(c)) {
                            Some(ing) => {
                                slots.push(Some(out.len()));
                                out.push(ing.clone());
                            }
                            None => slots.push(None),
                        }
                    }
                    Some(place::Placement {
                        ingredients: out,
                        slots,
                        shaped: Some((*width, *height)),
                    })
                }
                Crafting::Shapeless { ingredients, .. } => Some(seq(ingredients.clone())),
                Crafting::Transmute {
                    input,
                    material,
                    max,
                    ..
                } => {
                    let mut list = vec![input.clone()];
                    for _ in 0..*max {
                        list.push(material.clone());
                    }
                    Some(seq(list))
                }
                Crafting::Imbue {
                    source, material, ..
                } => {
                    let mut list = vec![material.clone(); 9];
                    if let Some(c) = list.get_mut(4) {
                        *c = source.clone();
                    }
                    Some(seq(list))
                }
                Crafting::Dye { target, dye, .. } => Some(seq(vec![target.clone(), dye.clone()])),
                _ => None,
            },
            book::RecipeRef::Cooking(i) => Some(seq(vec![self.cooking.get(i)?.ingredient.clone()])),
            // Stonecutter and smithing recipes have no recipe book menu.
            book::RecipeRef::Stonecutting(_) | book::RecipeRef::Smithing(_) => None,
        }
    }

    /// `recipeMatches` of a crafting recipe on a grid.
    pub fn crafting_matches(&self, index: usize, input: &CraftingInput) -> bool {
        input.ingredient_count > 0
            && self
                .crafting
                .get(index)
                .is_some_and(|r| r.kind.matches(input))
    }

    /// `getRecipeFor(CRAFTING, input)`: the first crafting recipe that matches, with its
    /// index.
    pub fn crafting_for(&self, input: &CraftingInput) -> Option<usize> {
        self.crafting_for_in(input, &|_| None)
    }

    /// [`Self::crafting_for`] with the level's map data: `map_scale` is the scale of a filled
    /// map's data (`MapItem.getSavedData(map, level).scale`), which `MapExtendingRecipe`
    /// needs to be below [`crate::map::MAX_SCALE`].
    pub fn crafting_for_in(
        &self,
        input: &CraftingInput,
        map_scale: &dyn Fn(&ItemStack) -> Option<i8>,
    ) -> Option<usize> {
        if input.ingredient_count == 0 {
            return None;
        }
        self.crafting.iter().position(|r| {
            r.kind.matches(input)
                && match r.kind {
                    Crafting::MapExtending { .. } => filled_map(input)
                        .and_then(map_scale)
                        .is_some_and(|scale| scale < crate::map::MAX_SCALE),
                    _ => true,
                }
        })
    }

    /// `assemble` of the crafting recipe `index`.
    pub fn assemble(&self, index: usize, input: &CraftingInput) -> ItemStack {
        self.crafting
            .get(index)
            .map(|r| r.kind.assemble(input))
            .unwrap_or_default()
    }

    /// `getRemainingItems` of the crafting recipe `index` (or the defaults without one).
    pub fn remainders(&self, index: Option<usize>, input: &CraftingInput) -> Vec<ItemStack> {
        match index.and_then(|i| self.crafting.get(i)) {
            Some(CraftingRecipe {
                kind: Crafting::BookCloning { .. },
                ..
            }) => {
                // The original book stays.
                let mut out = default_remainders(input);
                for (slot, stack) in out.iter_mut().zip(&input.items) {
                    if stack.is_empty() && !slot.is_empty() {
                        continue;
                    }
                    if !slot.is_empty() {
                        continue;
                    }
                    if item::item_id(stack) == vanillite_data::item::ids::WRITTEN_BOOK {
                        *slot = stack.with_count(1);
                    }
                }
                out
            }
            Some(CraftingRecipe {
                kind: Crafting::BannerDuplicate { .. },
                ..
            }) => {
                // `BannerDuplicateRecipe.getRemainingItems`: the banner with patterns stays.
                let mut out = default_remainders(input);
                for (slot, stack) in out.iter_mut().zip(&input.items) {
                    if slot.is_empty() && banner_layers(stack) != 0 {
                        *slot = stack.with_count(1);
                    }
                }
                out
            }
            _ => default_remainders(input),
        }
    }

    /// The first cooking recipe of `kind` for `input`.
    pub fn cooking_for(&self, kind: CookingKind, input: &ItemStack) -> Option<&Cooking> {
        if input.is_empty() {
            return None;
        }
        self.cooking
            .iter()
            .find(|r| r.kind == kind && r.ingredient.test(input))
    }

    /// Whether any cooking recipe of `kind` takes `input` (`isValidInput`).
    pub fn is_cooking_input(&self, kind: CookingKind, input: &ItemStack) -> bool {
        self.cooking_for(kind, input).is_some()
    }

    /// The fuel value of `stack` in a furnace; `fast` for blast furnaces and smokers.
    pub fn fuel(&self, stack: &ItemStack, fast: bool) -> Option<Fuel> {
        if stack.is_empty() {
            return None;
        }
        // `FuelValues.burnDuration`: the stack's own `cooking_fuel` (patched or removed).
        use vanillite_protocol::components::ids::COOKING_FUEL;
        if let Some(p) = stack.patch() {
            if let Some(v) = p.get(COOKING_FUEL) {
                let mut r = vanillite_protocol::codec::Reader::new(v);
                let burn = read_resolvable(&mut r, false)?;
                let speed = read_resolvable(&mut r, true)?;
                let ctx = ProviderContext {
                    ints: &self.providers.0,
                    floats: &self.providers.1,
                    fast,
                };
                return Some(Fuel {
                    burn_time: ctx.int(&burn, 0),
                    speed: ctx.float(&speed, 0),
                })
                .filter(|f| f.burn_time > 0);
            }
            if p.is_removed(COOKING_FUEL) {
                return None;
            }
        }
        let table = if fast {
            &self.fuel_fast
        } else {
            &self.fuel_normal
        };
        table.get(&stack.item).copied()
    }

    /// The smithing property sets: template, base and addition items, sorted.
    pub fn smithing_sets(&self) -> (&[u16], &[u16], &[u16]) {
        (
            &self.smithing_sets.0,
            &self.smithing_sets.1,
            &self.smithing_sets.2,
        )
    }

    /// `PotionBrewing.mix`: the brewing recipe for a bottle and a reagent.
    pub fn brewing_for(&self, bottle: &ItemStack, reagent: &ItemStack) -> Option<&Brewing> {
        if bottle.is_empty() || reagent.is_empty() {
            return None;
        }
        let potion = potion_of(bottle);
        self.brewing.iter().find(|r| {
            r.input.test(bottle)
                && r.reagent.test(reagent)
                && r.input_potion.is_none_or(|p| Some(p) == potion)
        })
    }

    /// `PotionBrewing.isIngredient`: whether any recipe takes `stack` as its reagent.
    pub fn is_brewing_reagent(&self, stack: &ItemStack) -> bool {
        self.brewing.iter().any(|r| r.reagent.test(stack))
    }

    /// `ClientboundUpdateRecipesPacket`: the property sets and the stonecutter recipes.
    pub fn update_packet(&self) -> vanillite_protocol::packets::play::clientbound::UpdateRecipes {
        use vanillite_protocol::packets::play::clientbound::{StonecutterEntry, UpdateRecipes};
        let set = |items: &mut Vec<i32>, ingredient: &Ingredient| {
            items.extend(ingredient.items().iter().map(|&i| i32::from(i)));
        };
        let mut sets: Vec<(String, Vec<i32>)> = Vec::new();
        let mut add = |key: &str, items: Vec<i32>| {
            let mut items = items;
            items.sort_unstable();
            items.dedup();
            sets.push((format!("minecraft:{key}"), items));
        };
        for (key, kind) in [
            ("furnace_input", CookingKind::Smelting),
            ("blast_furnace_input", CookingKind::Blasting),
            ("smoker_input", CookingKind::Smoking),
            ("campfire_input", CookingKind::Campfire),
        ] {
            let mut items = Vec::new();
            for r in self.cooking.iter().filter(|r| r.kind == kind) {
                set(&mut items, &r.ingredient);
            }
            add(key, items);
        }
        let (mut base, mut template, mut addition) = (Vec::new(), Vec::new(), Vec::new());
        for (_, r) in &self.smithing {
            match r {
                Smithing::Transform {
                    template: t,
                    base: b,
                    addition: a,
                    ..
                } => {
                    if let Some(b) = b {
                        set(&mut base, b);
                    }
                    if let Some(t) = t {
                        set(&mut template, t);
                    }
                    if let Some(a) = a {
                        set(&mut addition, a);
                    }
                }
                Smithing::Trim {
                    template: t,
                    base: b,
                    addition: a,
                    ..
                } => {
                    set(&mut base, b);
                    set(&mut template, t);
                    set(&mut addition, a);
                }
            }
        }
        add("smithing_base", base);
        add("smithing_template", template);
        add("smithing_addition", addition);
        let (mut inputs, mut reagents) = (Vec::new(), Vec::new());
        for r in &self.brewing {
            set(&mut inputs, &r.input);
            set(&mut reagents, &r.reagent);
        }
        add("brewing_input", inputs);
        add("brewing_reagent", reagents);
        UpdateRecipes {
            item_sets: sets,
            stonecutter: self
                .stonecutting
                .iter()
                .map(|r| StonecutterEntry {
                    input: r.ingredient.items().iter().map(|&i| i32::from(i)).collect(),
                    result: r.result.clone(),
                })
                .collect(),
        }
    }

    /// The stonecutter recipes for `input`, in id order.
    pub fn stonecutting_for<'a>(
        &'a self,
        input: &'a ItemStack,
    ) -> impl Iterator<Item = &'a Stonecutting> + 'a {
        self.stonecutting
            .iter()
            .filter(move |r| r.ingredient.test(input))
    }
}

impl Crafting {
    /// `matches(input, level)`.
    pub fn matches(&self, input: &CraftingInput) -> bool {
        match self {
            Crafting::Shaped {
                width,
                height,
                cells,
                ingredients,
                symmetrical,
                ..
            } => {
                let count = cells.iter().filter(|c| c.is_some()).count();
                if input.ingredient_count != count
                    || input.width != *width
                    || input.height != *height
                {
                    return false;
                }
                let test = |flip: bool| {
                    for y in 0..*height {
                        for x in 0..*width {
                            let cx = if flip { width - x - 1 } else { x };
                            let expected = cells.get(cx + y * width).copied().flatten();
                            let stack = input.get(x, y);
                            let ok = match expected.and_then(|i| ingredients.get(i)) {
                                Some(ing) => ing.test(stack),
                                None => stack.is_empty(),
                            };
                            if !ok {
                                return false;
                            }
                        }
                    }
                    true
                };
                (!symmetrical && test(true)) || test(false)
            }
            Crafting::Shapeless { ingredients, .. } => {
                if input.ingredient_count != ingredients.len() {
                    return false;
                }
                let stacks: Vec<&ItemStack> = input.non_empty().collect();
                if let ([only], [ing]) = (&stacks[..], &ingredients[..]) {
                    return ing.test(only);
                }
                can_assign(ingredients, &stacks)
            }
            Crafting::Transmute {
                input: target,
                material,
                min,
                max,
                add_material_count,
                result,
            } => {
                let n = i32::try_from(input.ingredient_count).unwrap_or(i32::MAX);
                if n < min + 1 || n > max + 1 {
                    return false;
                }
                let mut found: Option<&ItemStack> = None;
                let mut materials = 0;
                for stack in input.non_empty() {
                    if target.test(stack) {
                        if found.is_some() {
                            return false;
                        }
                        found = Some(stack);
                    } else {
                        if !material.test(stack) {
                            return false;
                        }
                        materials += 1;
                        if materials > *max {
                            return false;
                        }
                    }
                }
                let Some(found) = found else {
                    return false;
                };
                if materials < *min || materials > *max {
                    return false;
                }
                let size = if *add_material_count {
                    materials + result.count
                } else {
                    result.count
                };
                if size != 1 {
                    return true;
                }
                let out = transmute(result, found, 0);
                !out.is_empty() && !out.same_item_same_components(found)
            }
            Crafting::Imbue {
                source, material, ..
            } => {
                if input.width != 3 || input.height != 3 || input.ingredient_count != 9 {
                    return false;
                }
                (0..3).all(|y| {
                    (0..3).all(|x| {
                        let ing = if x == 1 && y == 1 { source } else { material };
                        ing.test(input.get(x, y))
                    })
                })
            }
            Crafting::DecoratedPot {
                back,
                left,
                right,
                front,
                ..
            } => {
                input.width == 3
                    && input.height == 3
                    && input.ingredient_count == 4
                    && back.test(input.get(1, 0))
                    && left.test(input.get(0, 1))
                    && right.test(input.get(2, 1))
                    && front.test(input.get(1, 2))
            }
            Crafting::RepairItem => repair_pair(input).is_some(),
            Crafting::BookCloning {
                source, material, ..
            } => {
                if input.ingredient_count < 2 {
                    return false;
                }
                let mut has_source = false;
                let mut has_material = false;
                for stack in input.non_empty() {
                    if source.test(stack) {
                        if has_source {
                            return false;
                        }
                        has_source = true;
                    } else if material.test(stack) {
                        has_material = true;
                    } else {
                        return false;
                    }
                }
                has_source && has_material
            }
            Crafting::Dye { target, dye, .. } => {
                if input.ingredient_count < 2 {
                    return false;
                }
                let mut has_target = false;
                let mut has_dyes = false;
                for stack in input.non_empty() {
                    if target.test(stack) {
                        if has_target {
                            return false;
                        }
                        has_target = true;
                    } else {
                        if !dye.test(stack) || crate::menu::dye_color(stack).is_none() {
                            return false;
                        }
                        has_dyes = true;
                    }
                }
                has_dyes && has_target
            }
            Crafting::BannerDuplicate { banner, .. } => {
                if input.ingredient_count != 2 {
                    return false;
                }
                let mut color = None;
                let mut has_target = false;
                let mut has_source = false;
                for stack in input.non_empty() {
                    let Some(c) = banner_color(stack).filter(|_| banner.test(stack)) else {
                        return false;
                    };
                    if color.is_some_and(|x| x != c) {
                        return false;
                    }
                    color = Some(c);
                    let patterns = banner_layers(stack);
                    if patterns > 6 {
                        return false;
                    }
                    if patterns > 0 {
                        if has_source {
                            return false;
                        }
                        has_source = true;
                    } else {
                        if has_target {
                            return false;
                        }
                        has_target = true;
                    }
                }
                has_source && has_target
            }
            Crafting::ShieldDecoration { banner, target, .. } => {
                if input.ingredient_count != 2 {
                    return false;
                }
                let mut has_target = false;
                let mut has_banner = false;
                for stack in input.non_empty() {
                    if banner.test(stack) && banner_color(stack).is_some() {
                        if has_banner {
                            return false;
                        }
                        has_banner = true;
                    } else {
                        if !target.test(stack) || has_target || banner_layers(stack) != 0 {
                            return false;
                        }
                        has_target = true;
                    }
                }
                has_target && has_banner
            }
            Crafting::MapExtending { map, material, .. } => {
                // `ShapedRecipePattern.of('#' material, 'x' map, "###", "#x#", "###")`.
                input.width == 3
                    && input.height == 3
                    && input.ingredient_count == 9
                    && (0..3).all(|y| {
                        (0..3).all(|x| {
                            let ing = if x == 1 && y == 1 { map } else { material };
                            ing.test(input.get(x, y))
                        })
                    })
                    && filled_map(input).is_some()
            }
            Crafting::FireworkStar {
                shapes,
                trail,
                twinkle,
                fuel,
                dye,
                ..
            } => {
                if input.ingredient_count < 2 {
                    return false;
                }
                let (mut has_fuel, mut has_dye, mut has_shape, mut has_trail, mut has_twinkle) =
                    (false, false, false, false, false);
                for stack in input.non_empty() {
                    if twinkle.test(stack) {
                        if has_twinkle {
                            return false;
                        }
                        has_twinkle = true;
                    } else if trail.test(stack) {
                        if has_trail {
                            return false;
                        }
                        has_trail = true;
                    } else if fuel.test(stack) {
                        if has_fuel {
                            return false;
                        }
                        has_fuel = true;
                    } else if dye.test(stack) && crate::menu::dye_color(stack).is_some() {
                        has_dye = true;
                    } else {
                        if !shapes.iter().any(|(_, i)| i.test(stack)) || has_shape {
                            return false;
                        }
                        has_shape = true;
                    }
                }
                has_fuel && has_dye
            }
            Crafting::FireworkStarFade { target, dye, .. } => {
                if input.ingredient_count < 2 {
                    return false;
                }
                let mut has_dye = false;
                let mut has_target = false;
                for stack in input.non_empty() {
                    if dye.test(stack) && crate::menu::dye_color(stack).is_some() {
                        has_dye = true;
                    } else {
                        if !target.test(stack) || has_target {
                            return false;
                        }
                        has_target = true;
                    }
                }
                has_target && has_dye
            }
            Crafting::FireworkRocket {
                shell, fuel, star, ..
            } => {
                if input.ingredient_count < 2 {
                    return false;
                }
                let mut has_shell = false;
                let mut fuels = 0;
                for stack in input.non_empty() {
                    if shell.test(stack) {
                        if has_shell {
                            return false;
                        }
                        has_shell = true;
                    } else if fuel.test(stack) {
                        fuels += 1;
                        if fuels > 3 {
                            return false;
                        }
                    } else if !star.test(stack) {
                        return false;
                    }
                }
                has_shell && fuels >= 1
            }
        }
    }

    /// `assemble(input)`.
    pub fn assemble(&self, input: &CraftingInput) -> ItemStack {
        match self {
            Crafting::Shaped { result, .. } | Crafting::Shapeless { result, .. } => result.clone(),
            Crafting::Transmute {
                input: target,
                material,
                add_material_count,
                result,
                ..
            } => {
                let mut materials = 0;
                let mut found = None;
                for stack in input.non_empty() {
                    if target.test(stack) {
                        found = Some(stack);
                        if !add_material_count {
                            break;
                        }
                    } else if material.test(stack) {
                        materials += 1;
                    }
                }
                match found {
                    Some(f) => {
                        transmute(result, f, if *add_material_count { materials } else { 0 })
                    }
                    None => ItemStack::EMPTY,
                }
            }
            Crafting::Imbue { result, .. } => {
                let mut out = result.clone();
                if let Some(potion) = input.get(1, 1).component(components::POTION_CONTENTS) {
                    out.set_component(
                        components::POTION_CONTENTS,
                        potion.to_vec().into_boxed_slice(),
                    );
                }
                out
            }
            Crafting::DecoratedPot { result, .. } => {
                let mut out = result.clone();
                // `PotDecorations`: back, left, right, front as optional item stack templates
                // of one item.
                let mut bytes = Vec::new();
                for (x, y) in [(1, 0), (0, 1), (2, 1), (1, 2)] {
                    input
                        .get(x, y)
                        .with_count(1)
                        .encode_template_optional(&mut bytes);
                }
                out.set_component(components::POT_DECORATIONS, bytes.into_boxed_slice());
                out
            }
            Crafting::RepairItem => {
                let Some((first, second)) = repair_pair(input) else {
                    return ItemStack::EMPTY;
                };
                let durability = item::max_damage(first).max(item::max_damage(second));
                let remaining1 = item::max_damage(first) - item::damage(first);
                let remaining2 = item::max_damage(second) - item::damage(second);
                let remaining = remaining1 + remaining2 + durability * 5 / 100;
                let mut out = ItemStack::new(first.item, 1);
                if durability != item::max_damage(&out) {
                    let mut bytes = Vec::new();
                    write_varint(&mut bytes, durability);
                    out.set_component(components::MAX_DAMAGE, bytes.into_boxed_slice());
                }
                item::set_damage(&mut out, (durability - remaining).max(0));
                // The curses of both (`#minecraft:curse`) at the higher level.
                if let Some(d) = crate::player::play::game_data() {
                    use crate::menu::workstation::{
                        crafting_enchantments, set_crafting_enchantments,
                    };
                    let (a, b) = (crafting_enchantments(first), crafting_enchantments(second));
                    let level = |l: &[(u32, i32)], id: u32| {
                        l.iter().find(|(e, _)| *e == id).map_or(0, |(_, v)| *v)
                    };
                    let mut list: Vec<(u32, i32)> = Vec::new();
                    for &(id, _) in a.iter().chain(&b) {
                        if d.enchanting.is_curse(id) && list.iter().all(|(e, _)| *e != id) {
                            list.push((id, level(&a, id).max(level(&b, id))));
                        }
                    }
                    set_crafting_enchantments(&mut out, &list);
                }
                out
            }
            Crafting::BookCloning { result, .. } => {
                // The copy count is the number of material books; the content needs the
                // written book component, which is copied as it is.
                let mut copies = 0;
                let mut source = None;
                for stack in input.non_empty() {
                    if item::item_id(stack) == vanillite_data::item::ids::WRITTEN_BOOK {
                        source = Some(stack);
                    } else {
                        copies += 1;
                    }
                }
                let Some(source) = source else {
                    return ItemStack::EMPTY;
                };
                let Some(content) = source.component(components::WRITTEN_BOOK_CONTENT) else {
                    return ItemStack::EMPTY;
                };
                let Some(copy) = book_copy(content) else {
                    return ItemStack::EMPTY;
                };
                // `TransmuteRecipe.createWithOriginalComponents(result, source, copies - 1)`:
                // the source's other components (name, lore, …) come along.
                let mut out = result.with_count(result.count.saturating_add(copies - 1));
                if let Some(patch) = source.patch() {
                    for (t, v) in &patch.added {
                        out.set_component(*t, v.clone());
                    }
                    for t in &patch.removed {
                        out.remove_component(*t);
                    }
                }
                out.set_component(components::WRITTEN_BOOK_CONTENT, copy.into_boxed_slice());
                out
            }
            Crafting::Dye {
                target,
                dye,
                result,
            } => {
                let mut dyes = Vec::new();
                let mut found: Option<&ItemStack> = None;
                for stack in input.non_empty() {
                    if target.test(stack) {
                        if found.is_some() {
                            return ItemStack::EMPTY;
                        }
                        found = Some(stack);
                    } else {
                        if !dye.test(stack) {
                            return ItemStack::EMPTY;
                        }
                        dyes.push(crate::menu::dye_color(stack).unwrap_or(0));
                    }
                }
                let Some(found) = found.filter(|_| !dyes.is_empty()) else {
                    return ItemStack::EMPTY;
                };
                let current = found
                    .component(components::DYED_COLOR)
                    .and_then(|v| vanillite_protocol::codec::Reader::new(v).i32().ok());
                let rgb = apply_dyes(current, &dyes);
                let mut out = transmute(result, found, 0);
                out.set_component(components::DYED_COLOR, Box::new(rgb.to_be_bytes()));
                out
            }
            Crafting::BannerDuplicate { result, .. } => input
                .non_empty()
                .find(|s| (1..=6).contains(&banner_layers(s)))
                .map_or(ItemStack::EMPTY, |s| transmute(result, s, 0)),
            Crafting::ShieldDecoration {
                banner,
                target,
                result,
            } => {
                let mut patterns = None;
                let mut base = 0;
                let mut found = &ItemStack::EMPTY;
                for stack in input.non_empty() {
                    if let Some(color) = banner_color(stack).filter(|_| banner.test(stack)) {
                        patterns = stack.component(components::BANNER_PATTERNS);
                        base = color;
                    } else if target.test(stack) {
                        found = stack;
                    }
                }
                let mut out = transmute(result, found, 0);
                match patterns {
                    Some(p) => out
                        .set_component(components::BANNER_PATTERNS, p.to_vec().into_boxed_slice()),
                    None => out.patch_mut().unset(components::BANNER_PATTERNS),
                }
                let mut color = Vec::with_capacity(1);
                write_varint(&mut color, base);
                out.set_component(components::BASE_COLOR, color.into_boxed_slice());
                out
            }
            Crafting::MapExtending { result, .. } => {
                let Some(source) = filled_map(input) else {
                    return ItemStack::EMPTY;
                };
                let mut out = transmute(result, source, 0);
                crate::map::set_post_processing(&mut out, crate::map::PostProcessing::Scale);
                out
            }
            Crafting::FireworkStar {
                shapes,
                trail,
                twinkle,
                dye,
                result,
                ..
            } => {
                let mut shape = 0;
                let mut has_twinkle = false;
                let mut has_trail = false;
                let mut colors = Vec::new();
                for stack in input.non_empty() {
                    if let Some((s, _)) = shapes.iter().find(|(_, i)| i.test(stack)) {
                        shape = *s;
                    } else if twinkle.test(stack) {
                        has_twinkle = true;
                    } else if trail.test(stack) {
                        has_trail = true;
                    } else if dye.test(stack) {
                        colors.push(firework_color(crate::menu::dye_color(stack).unwrap_or(0)));
                    }
                }
                let mut out = result.clone();
                let mut bytes = Vec::new();
                write_explosion(&mut bytes, shape, &colors, &[], has_trail, has_twinkle);
                out.set_component(components::FIREWORK_EXPLOSION, bytes.into_boxed_slice());
                out
            }
            Crafting::FireworkStarFade {
                target,
                dye,
                result,
            } => {
                let mut colors = Vec::new();
                let mut found = None;
                for stack in &input.items {
                    if dye.test(stack) {
                        colors.push(firework_color(crate::menu::dye_color(stack).unwrap_or(0)));
                    } else if target.test(stack) {
                        found = Some(stack);
                    }
                }
                let Some(found) = found.filter(|_| !colors.is_empty()) else {
                    return ItemStack::EMPTY;
                };
                let mut out = found.clone();
                out.item = result.item;
                out.count = result.count;
                let mut bytes = Vec::new();
                match found
                    .component(components::FIREWORK_EXPLOSION)
                    .and_then(read_explosion)
                {
                    Some((shape, old_colors, _, trail, twinkle)) => {
                        write_explosion(&mut bytes, shape, &old_colors, &colors, trail, twinkle);
                    }
                    None => write_explosion(&mut bytes, 0, &[], &colors, false, false),
                }
                out.set_component(components::FIREWORK_EXPLOSION, bytes.into_boxed_slice());
                out
            }
            Crafting::FireworkRocket {
                fuel, star, result, ..
            } => {
                let mut fuels = 0;
                let mut explosions: Vec<&[u8]> = Vec::new();
                for stack in input.non_empty() {
                    if fuel.test(stack) {
                        fuels += 1;
                    } else if star.test(stack)
                        && let Some(e) = stack.component(components::FIREWORK_EXPLOSION)
                    {
                        explosions.push(e);
                    }
                }
                let mut bytes = Vec::new();
                write_varint(&mut bytes, fuels);
                write_varint(&mut bytes, i32::try_from(explosions.len()).unwrap_or(0));
                for e in explosions {
                    bytes.extend_from_slice(e);
                }
                let mut out = result.clone();
                out.set_component(components::FIREWORKS, bytes.into_boxed_slice());
                out
            }
        }
    }
}

/// `FireworkExplosion.Shape` names by id.
const FIREWORK_SHAPES: [&str; 5] = ["small_ball", "large_ball", "star", "creeper", "burst"];

/// `DyeColor.getTextureDiffuseColor` (without alpha) by id.
const DYE_DIFFUSE: [i32; 16] = [
    16383998, 16351261, 13061821, 3847130, 16701501, 8439583, 15961002, 4673362, 10329495, 1481884,
    8991416, 3949738, 8606770, 6192150, 11546150, 1908001,
];
/// `DyeColor.getFireworkColor` by id.
const DYE_FIREWORK: [i32; 16] = [
    15790320, 15435844, 12801229, 6719955, 14602026, 4312372, 14188952, 4408131, 11250603, 2651799,
    8073150, 2437522, 5320730, 3887386, 11743532, 1973019,
];

/// `CelebrateVillagersSurvivedRaid.getFirework`: a rocket of `flight` with one burst of the
/// dye colour `dye`.
pub(crate) fn celebration_rocket(dye: i32, flight: i32) -> ItemStack {
    let mut bytes = Vec::new();
    write_varint(&mut bytes, flight);
    write_varint(&mut bytes, 1);
    write_explosion(&mut bytes, 4, &[firework_color(dye)], &[], false, false);
    let mut out = ItemStack::new(vanillite_data::item::ids::FIREWORK_ROCKET.0, 1);
    out.set_component(components::FIREWORKS, bytes.into_boxed_slice());
    out
}

fn firework_color(dye: i32) -> i32 {
    usize::try_from(dye)
        .ok()
        .and_then(|i| DYE_FIREWORK.get(i))
        .copied()
        .unwrap_or(0)
}

/// `DyedItemColor.applyDyes(current, dyes)`.
pub(crate) fn apply_dyes(current: Option<i32>, dyes: &[i32]) -> i32 {
    let (mut red, mut green, mut blue, mut intensity, mut n) = (0i32, 0i32, 0i32, 0i32, 0i32);
    let mut add = |rgb: i32| {
        let (r, g, b) = ((rgb >> 16) & 255, (rgb >> 8) & 255, rgb & 255);
        intensity += r.max(g).max(b);
        red += r;
        green += g;
        blue += b;
        n += 1;
    };
    if let Some(c) = current {
        add(c);
    }
    for &d in dyes {
        add(usize::try_from(d)
            .ok()
            .and_then(|i| DYE_DIFFUSE.get(i))
            .copied()
            .unwrap_or(0));
    }
    let n = n.max(1);
    let (r, g, b) = (red / n, green / n, blue / n);
    let average = intensity as f32 / n as f32;
    let max = r.max(g).max(b) as f32;
    let scale = |c: i32| (c as f32 * average / max) as i32;
    (scale(r) << 16) | (scale(g) << 8) | scale(b)
}

/// The colour of a banner item (`BannerItem.getColor`), `None` for other items.
fn banner_color(stack: &ItemStack) -> Option<i32> {
    let block = item::item_id(stack).block()?;
    if block.info().kind != vanillite_data::block::BlockKind::BannerBlock {
        return None;
    }
    let name = block
        .name()
        .strip_prefix("minecraft:")?
        .strip_suffix("_banner")?;
    let color = ItemStack::new(
        vanillite_data::registry::ITEM
            .id(&format!("minecraft:{name}_dye"))
            .unwrap_or(0),
        1,
    );
    crate::menu::dye_color(&color)
}

/// Number of `banner_patterns` layers of a stack.
fn banner_layers(stack: &ItemStack) -> usize {
    stack
        .component(components::BANNER_PATTERNS)
        .and_then(|v| vanillite_protocol::codec::Reader::new(v).varint().ok())
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

/// `MapExtendingRecipe.findFilledMap`.
fn filled_map(input: &CraftingInput) -> Option<&ItemStack> {
    input
        .items
        .iter()
        .find(|s| s.component(components::MAP_ID).is_some())
}

/// A `FireworkExplosion` value.
fn write_explosion(
    out: &mut Vec<u8>,
    shape: i32,
    colors: &[i32],
    fades: &[i32],
    trail: bool,
    twinkle: bool,
) {
    write_varint(out, shape);
    for list in [colors, fades] {
        write_varint(out, i32::try_from(list.len()).unwrap_or(0));
        for c in list {
            out.extend_from_slice(&c.to_be_bytes());
        }
    }
    out.push(u8::from(trail));
    out.push(u8::from(twinkle));
}

/// Reads a `FireworkExplosion` value.
#[allow(clippy::type_complexity)]
fn read_explosion(value: &[u8]) -> Option<(i32, Vec<i32>, Vec<i32>, bool, bool)> {
    let mut r = vanillite_protocol::codec::Reader::new(value);
    let shape = r.varint().ok()?;
    let mut lists = [Vec::new(), Vec::new()];
    for list in &mut lists {
        let n = r.len_prefix("colors", 1024, 4).ok()?;
        for _ in 0..n {
            list.push(r.i32().ok()?);
        }
    }
    let trail = r.u8().ok()? != 0;
    let twinkle = r.u8().ok()? != 0;
    let [colors, fades] = lists;
    Some((shape, colors, fades, trail, twinkle))
}

/// The `result` of a recipe whose result keeps the input's components (`TransmuteResult`).
fn transmute_result(json: &Value) -> Result<TransmuteResult, String> {
    let result = json.get("result").ok_or("no result")?;
    Ok(match result {
        Value::String(s) => TransmuteResult {
            item: Some(item_id(s)?),
            count: 1,
        },
        v => TransmuteResult {
            item: v
                .get("id")
                .and_then(Value::as_str)
                .map(item_id)
                .transpose()?,
            count: count_of(v),
        },
    })
}

/// `WrittenBookContent.tryCraftCopy`: the content one generation further, `None` past
/// "copy of copy". Layout: title (filterable string), author, generation varint, pages,
/// resolved flag.
fn book_copy(content: &[u8]) -> Option<Vec<u8>> {
    let mut r = vanillite_protocol::codec::Reader::new(content);
    r.string(32).ok()?;
    if r.take(1).ok()?.first().copied()? != 0 {
        r.string(32).ok()?;
    }
    r.string(32767).ok()?;
    let gen_pos = content.len() - r.remaining();
    let generation = r.varint().ok()?;
    let gen_end = content.len() - r.remaining();
    if generation >= 2 {
        return None;
    }
    let mut out = Vec::with_capacity(content.len());
    out.extend_from_slice(content.get(..gen_pos)?);
    write_varint(&mut out, generation + 1);
    out.extend_from_slice(content.get(gen_end..)?);
    Some(out)
}

/// `RepairItemRecipe.getItemsToCombine`: two single stacks of one item with `max_damage` (and
/// `damage`; unbreakable ones too).
fn repair_pair(input: &CraftingInput) -> Option<(&ItemStack, &ItemStack)> {
    if input.ingredient_count != 2 {
        return None;
    }
    let mut it = input.non_empty();
    let first = it.next()?;
    let second = it.next()?;
    let combinable = |s: &ItemStack| s.count == 1 && item::max_damage(s) > 0;
    (first.item == second.item && combinable(first) && combinable(second))
        .then_some((first, second))
}

/// `TransmuteRecipe.createWithOriginalComponents`.
fn transmute(result: &TransmuteResult, input: &ItemStack, extra: i32) -> ItemStack {
    let item = result.item.unwrap_or(input.item);
    let mut out = input.clone();
    out.item = item;
    out.count = result.count.saturating_add(extra);
    out
}

/// `StackedItemContents.canCraft` for shapeless recipes: every ingredient gets its own stack.
fn can_assign(ingredients: &[Ingredient], stacks: &[&ItemStack]) -> bool {
    // Bipartite matching (at most 9 × 9) with augmenting paths.
    let mut owner: Vec<Option<usize>> = vec![None; stacks.len()];
    fn augment(
        i: usize,
        ingredients: &[Ingredient],
        stacks: &[&ItemStack],
        seen: &mut [bool],
        owner: &mut [Option<usize>],
    ) -> bool {
        for (s, stack) in stacks.iter().enumerate() {
            let Some(ing) = ingredients.get(i) else {
                return false;
            };
            if seen.get(s).copied().unwrap_or(true) || !ing.test(stack) {
                continue;
            }
            if let Some(v) = seen.get_mut(s) {
                *v = true;
            }
            let current = owner.get(s).copied().flatten();
            if current.is_none_or(|o| augment(o, ingredients, stacks, seen, owner)) {
                if let Some(o) = owner.get_mut(s) {
                    *o = Some(i);
                }
                return true;
            }
        }
        false
    }
    for i in 0..ingredients.len() {
        let mut seen = vec![false; stacks.len()];
        if !augment(i, ingredients, stacks, &mut seen, &mut owner) {
            return false;
        }
    }
    true
}

fn item_id(name: &str) -> Result<u16, String> {
    ITEM.id(name).ok_or_else(|| format!("unknown item {name}"))
}

fn count_of(v: &Value) -> i32 {
    v.get("count")
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(1)
}

/// An `Ingredient` (a holder set of items): `"id"`, `"#tag"` or a list of ids.
fn ingredient(v: &Value, tag: &dyn Fn(&str) -> Vec<u16>) -> Result<Ingredient, String> {
    let mut items: Vec<u16> = match v {
        Value::String(s) => match s.strip_prefix('#') {
            Some(t) => tag(t),
            None => vec![item_id(s)?],
        },
        Value::Array(list) => list
            .iter()
            .map(|e| {
                e.as_str()
                    .ok_or("bad ingredient")
                    .map_err(str::to_owned)
                    .and_then(item_id)
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("bad ingredient".to_owned()),
    };
    items.retain(|&i| i != 0);
    items.sort_unstable();
    items.dedup();
    if items.is_empty() {
        return Err("empty ingredient".to_owned());
    }
    Ok(Ingredient(items.into_boxed_slice()))
}

/// An `ItemStackTemplate`: `{"id", "count", "components"}`.
fn result(v: &Value) -> Result<ItemStack, String> {
    let id = match v {
        Value::String(s) => s.as_str(),
        _ => v.get("id").and_then(Value::as_str).ok_or("no result id")?,
    };
    let mut stack = ItemStack::new(item_id(id)?, count_of(v));
    if let Some(Value::Object(comps)) = v.get("components") {
        for (key, value) in comps {
            match key.as_str() {
                "minecraft:suspicious_stew_effects" => {
                    let list = value.as_array().ok_or("bad stew effects")?;
                    let mut bytes = Vec::new();
                    write_varint(&mut bytes, i32::try_from(list.len()).unwrap_or(0));
                    for e in list {
                        let effect = e
                            .get("id")
                            .and_then(Value::as_str)
                            .and_then(|n| MOB_EFFECT.id(n))
                            .ok_or("unknown effect")?;
                        write_varint(&mut bytes, i32::from(effect));
                        let duration = e
                            .get("duration")
                            .and_then(Value::as_i64)
                            .and_then(|n| i32::try_from(n).ok())
                            .unwrap_or(160);
                        write_varint(&mut bytes, duration);
                    }
                    stack.set_component(
                        components::SUSPICIOUS_STEW_EFFECTS,
                        bytes.into_boxed_slice(),
                    );
                }
                other => return Err(format!("unsupported result component {other}")),
            }
        }
    }
    Ok(stack)
}

/// `ShapedRecipePattern` from its key and pattern rows, shrunk to the used area.
fn shaped(json: &Value, tag: &dyn Fn(&str) -> Vec<u16>) -> Result<Crafting, String> {
    let rows: Vec<Vec<char>> = json
        .get("pattern")
        .and_then(Value::as_array)
        .ok_or("no pattern")?
        .iter()
        .map(|r| r.as_str().map(|s| s.chars().collect()).ok_or("bad row"))
        .collect::<Result<_, _>>()?;
    let keys = json.get("key").and_then(Value::as_object).ok_or("no key")?;
    let mut ingredients = Vec::new();
    let mut by_char = HashMap::new();
    for (k, v) in keys {
        let c = k.chars().next().ok_or("bad key")?;
        by_char.insert(c, ingredients.len());
        ingredients.push(ingredient(v, tag)?);
    }
    let width_full = rows.iter().map(Vec::len).max().unwrap_or(0);
    let used = |c: char| c != ' ';
    let first_col = (0..width_full)
        .find(|&x| rows.iter().any(|r| r.get(x).is_some_and(|&c| used(c))))
        .ok_or("empty pattern")?;
    let last_col = (0..width_full)
        .rev()
        .find(|&x| rows.iter().any(|r| r.get(x).is_some_and(|&c| used(c))))
        .unwrap_or(first_col);
    let first_row = rows
        .iter()
        .position(|r| r.iter().any(|&c| used(c)))
        .ok_or("empty pattern")?;
    let last_row = rows
        .iter()
        .rposition(|r| r.iter().any(|&c| used(c)))
        .unwrap_or(first_row);
    let width = last_col - first_col + 1;
    let height = last_row - first_row + 1;
    let mut cells = Vec::with_capacity(width * height);
    for row in rows.get(first_row..=last_row).unwrap_or_default() {
        for x in first_col..=last_col {
            let c = row.get(x).copied().unwrap_or(' ');
            cells.push(if used(c) {
                Some(*by_char.get(&c).ok_or_else(|| format!("unknown key {c}"))?)
            } else {
                None
            });
        }
    }
    let symmetrical = (0..height).all(|y| {
        (0..width / 2).all(|x| cells.get(x + y * width) == cells.get(width - x - 1 + y * width))
    });
    Ok(Crafting::Shaped {
        width,
        height,
        cells: cells.into_boxed_slice(),
        ingredients,
        symmetrical,
        result: result(json.get("result").ok_or("no result")?)?,
    })
}

/// Evaluates context int and float providers for furnace fuels: numbers, references,
/// `div`, `conditional` with the `block/fast_cooking` predicate.
struct ProviderContext<'a> {
    ints: &'a HashMap<String, Value>,
    floats: &'a HashMap<String, Value>,
    /// The furnace is a blast furnace or smoker.
    fast: bool,
}

impl ProviderContext<'_> {
    fn condition(&self, v: &Value) -> bool {
        match v.as_str() {
            Some("minecraft:block/fast_cooking") => self.fast,
            _ => false,
        }
    }

    fn int(&self, v: &Value, depth: u32) -> i32 {
        if depth > 16 {
            return 0;
        }
        match v {
            Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
            Value::String(id) => self
                .ints
                .get(id)
                .map_or(0, |v| self.int(v, depth.saturating_add(1))),
            Value::Object(_) => match v.get("type").and_then(Value::as_str) {
                Some("minecraft:div") => {
                    let left = v.get("left").map_or(0, |l| self.int(l, depth + 1));
                    let right = v.get("right").map_or(1, |r| self.int(r, depth + 1));
                    if right == 0 { 0 } else { left / right }
                }
                Some("minecraft:conditional") => {
                    let branch = if v.get("condition").is_some_and(|c| self.condition(c)) {
                        "on_true"
                    } else {
                        "on_false"
                    };
                    v.get(branch).map_or(0, |b| self.int(b, depth + 1))
                }
                Some("minecraft:constant") => v.get("value").map_or(0, |b| self.int(b, depth + 1)),
                _ => 0,
            },
            _ => 0,
        }
    }

    fn float(&self, v: &Value, depth: u32) -> f32 {
        if depth > 16 {
            return 1.0;
        }
        #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
        match v {
            Value::Number(n) => n.as_f64().unwrap_or(1.0) as f32,
            Value::String(id) => self
                .floats
                .get(id)
                .map_or(1.0, |v| self.float(v, depth.saturating_add(1))),
            Value::Object(_) => match v.get("type").and_then(Value::as_str) {
                Some("minecraft:conditional") => {
                    let branch = if v.get("condition").is_some_and(|c| self.condition(c)) {
                        "on_true"
                    } else {
                        "on_false"
                    };
                    v.get(branch).map_or(1.0, |b| self.float(b, depth + 1))
                }
                Some("minecraft:constant") => {
                    v.get("value").map_or(1.0, |b| self.float(b, depth + 1))
                }
                _ => 1.0,
            },
            _ => 1.0,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod fuel_component_tests {
    use super::*;

    /// `FuelValues.burnDuration` reads the stack's `cooking_fuel` (I6-35 of #272): a patched
    /// value (number or provider id) replaces the item's, a removed one makes it no fuel.
    #[test]
    fn patched_and_removed_cooking_fuel() {
        use vanillite_protocol::components::ids::COOKING_FUEL;
        let mut recipes = Recipes::default();
        recipes
            .providers
            .0
            .insert("minecraft:test/burn".into(), Value::from(250));
        let coal = ITEM.id("minecraft:coal").unwrap();
        recipes.fuel_normal.insert(
            coal,
            Fuel {
                burn_time: 1600,
                speed: 1.0,
            },
        );
        let plain = ItemStack::new(coal, 1);
        assert_eq!(recipes.fuel(&plain, false).unwrap().burn_time, 1600);
        // Burn time: the provider `minecraft:test/burn`; speed: the number 2.0.
        let mut wire = vec![0u8];
        vanillite_protocol::codec::write_str(&mut wire, "minecraft:test/burn");
        wire.push(1);
        wire.extend_from_slice(&2.0f32.to_be_bytes());
        let mut patched = plain.clone();
        patched.patch_mut().set(COOKING_FUEL, wire.into());
        let fuel = recipes.fuel(&patched, false).unwrap();
        assert_eq!((fuel.burn_time, fuel.speed), (250, 2.0));
        let mut stick = ItemStack::new(ITEM.id("minecraft:stick").unwrap(), 1);
        let mut constant = vec![1u8];
        constant.extend_from_slice(&40i32.to_be_bytes());
        constant.push(1);
        constant.extend_from_slice(&1.0f32.to_be_bytes());
        stick.patch_mut().set(COOKING_FUEL, constant.into());
        assert_eq!(recipes.fuel(&stick, true).unwrap().burn_time, 40);
        let mut removed = plain;
        removed.remove_component(COOKING_FUEL);
        assert!(recipes.fuel(&removed, false).is_none());
    }
}
