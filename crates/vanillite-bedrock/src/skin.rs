//! Player skins on the Bedrock side.
//!
//! Bedrock players keep the skin their client sent (shown to other Bedrock players). Java players
//! start with a plain built-in skin; their Mojang skin is fetched from the texture server named
//! in their signed `textures` property and replaces it once loaded.
//!
//! Java players see Bedrock players with the default skin: Java clients only show skins signed by
//! Mojang, and a Bedrock skin has no such signature (uploading it to a signing service would send
//! player data to a third party), so the Java side keeps the default Steve/Alex look.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use crate::io;
use crate::login::field;

/// A skin as `player_list` carries it.
#[derive(Debug, Clone)]
pub struct Skin {
    pub id: String,
    pub resource_patch: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub geometry: Vec<u8>,
    pub geometry_version: Vec<u8>,
    pub slim: bool,
}

/// The classic humanoid model.
const DEFAULT_PATCH: &str = r#"{"geometry":{"default":"geometry.humanoid.custom"}}"#;
/// The slim model.
const SLIM_PATCH: &str = r#"{"geometry":{"default":"geometry.humanoid.customSlim"}}"#;

/// The player models the patches above name (`geometry.humanoid.custom` and `...customSlim`).
/// Clients need them in the skin's geometry field: since Bedrock 1.21.90 an empty field shows
/// the default skin, and since 1.26.60 the client rejects the whole `player_list` packet because
/// the field must hold valid JSON.
const HUMANOID_GEOMETRY: &str = include_str!("humanoid_geometry.json");

/// Deepest JSON nesting a skin field may have (the client rejects deeper ones; serde_json's own
/// recursion limit is 128).
const MAX_JSON_DEPTH: usize = 64;

/// `bytes` if it is a JSON object no deeper than [`MAX_JSON_DEPTH`].
fn json_object(bytes: Vec<u8>) -> Option<Vec<u8>> {
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    (v.is_object() && depth(&v) <= MAX_JSON_DEPTH).then_some(bytes)
}

/// Nesting depth of a JSON value (scalars 0).
fn depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => a.iter().map(depth).max().unwrap_or(0).saturating_add(1),
        Value::Object(o) => o.values().map(depth).max().unwrap_or(0).saturating_add(1),
        _ => 0,
    }
}

/// The model of player heads: the head cube and the hat layer around the origin.
const SKULL_PATCH: &str = r#"{"geometry":{"default":"geometry.vanillite.skull"}}"#;
const SKULL_GEOMETRY: &str = r#"{"format_version":"1.10.0","geometry.vanillite.skull":{"texturewidth":64,"textureheight":64,"visible_bounds_width":2,"visible_bounds_height":1,"visible_bounds_offset":[0,0,0],"bones":[{"name":"head","pivot":[0,24,0],"cubes":[{"origin":[-4,0,-4],"size":[8,8,8],"uv":[0,0]}]},{"name":"hat","parent":"head","pivot":[0,24,0],"cubes":[{"origin":[-4,0,-4],"size":[8,8,8],"uv":[32,0],"inflate":0.5}]}]}}"#;

/// Where Java skins may be loaded from.
const TEXTURE_HOST: &str = "http://textures.minecraft.net/texture/";

/// The skin URL and model (slim) of a Java profile's `textures` property.
pub fn java_texture(properties: &[vanillite_protocol::types::Property]) -> Option<(String, bool)> {
    let p = properties.iter().find(|p| p.name == "textures")?;
    texture_of(&p.value)
}

/// The skin URL and model (slim) of a `textures` property value.
pub fn texture_of(value: &str) -> Option<(String, bool)> {
    let json = STANDARD.decode(value.as_bytes()).ok()?;
    let v: Value = serde_json::from_slice(&json).ok()?;
    let skin = field(field(&v, "textures"), "SKIN");
    let url = skin.get("url")?.as_str()?;
    let hash = url
        .strip_prefix(TEXTURE_HOST)
        .or_else(|| url.strip_prefix("https://textures.minecraft.net/texture/"))?;
    if hash.is_empty() || hash.len() > 128 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let slim = field(field(skin, "metadata"), "model").as_str() == Some("slim");
    Some((format!("{TEXTURE_HOST}{hash}"), slim))
}

/// Loads a Java skin (blocking): 64x64 PNG from the texture server.
pub fn fetch_java(url: &str, slim: bool) -> Option<Skin> {
    if !url.starts_with(TEXTURE_HOST) {
        return None;
    }
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .build()
        .new_agent();
    let png = agent
        .get(url)
        .call()
        .ok()?
        .body_mut()
        .with_config()
        .limit(64 << 10)
        .read_to_vec()
        .ok()?;
    Skin::from_java_png(&png, slim, url.rsplit('/').next().unwrap_or("java"))
}

impl Skin {
    /// A plain 64x64 skin: skin-coloured head and arms, a shirt and trousers.
    pub fn plain() -> Self {
        let mut data = vec![0u8; 64 * 64 * 4];
        // Texture regions of the 64x64 layout (x, y, width, height, RGBA).
        let regions: [(usize, usize, usize, usize, [u8; 4]); 6] = [
            (0, 0, 32, 16, [0xc6, 0x8e, 0x6a, 0xff]),   // head
            (16, 16, 24, 16, [0x2f, 0x8f, 0xa8, 0xff]), // body
            (40, 16, 16, 16, [0xc6, 0x8e, 0x6a, 0xff]), // right arm
            (32, 48, 16, 16, [0xc6, 0x8e, 0x6a, 0xff]), // left arm
            (0, 16, 16, 16, [0x3b, 0x3b, 0x8f, 0xff]),  // right leg
            (16, 48, 16, 16, [0x3b, 0x3b, 0x8f, 0xff]), // left leg
        ];
        for (x0, y0, w, h, rgba) in regions {
            for y in y0..y0.saturating_add(h) {
                for x in x0..x0.saturating_add(w) {
                    let i = y.saturating_mul(64).saturating_add(x).saturating_mul(4);
                    if let Some(px) = data.get_mut(i..i.saturating_add(4)) {
                        px.copy_from_slice(&rgba);
                    }
                }
            }
        }
        Self {
            id: "vanillite.plain".into(),
            resource_patch: DEFAULT_PATCH.as_bytes().to_vec(),
            width: 64,
            height: 64,
            data,
            geometry: HUMANOID_GEOMETRY.as_bytes().to_vec(),
            geometry_version: b"0.0.0".to_vec(),
            slim: false,
        }
    }

    /// A Java skin image (64x64) as a Bedrock skin.
    pub fn from_java_png(png: &[u8], slim: bool, id: &str) -> Option<Self> {
        let (w, h, data) = crate::png::decode_rgba(png)?;
        if (w, h) != (64, 64) {
            return None;
        }
        Some(Self {
            id: format!("vanillite.java.{id}"),
            resource_patch: if slim { SLIM_PATCH } else { DEFAULT_PATCH }
                .as_bytes()
                .to_vec(),
            width: w,
            height: h,
            data,
            geometry: HUMANOID_GEOMETRY.as_bytes().to_vec(),
            geometry_version: b"0.0.0".to_vec(),
            slim,
        })
    }

    /// The head of this skin on a model of only the head and hat, for player heads (the model
    /// as Geyser (MIT) builds its skull skins).
    pub fn skull(&self) -> Self {
        Self {
            id: format!("{}.skull", self.id),
            resource_patch: SKULL_PATCH.as_bytes().to_vec(),
            width: self.width,
            height: self.height,
            data: self.data.clone(),
            geometry: SKULL_GEOMETRY.as_bytes().to_vec(),
            geometry_version: b"0.0.0".to_vec(),
            slim: false,
        }
    }

    /// The skin of a Bedrock client's login data, if it is well-formed.
    pub fn from_client_data(c: &Value) -> Option<Self> {
        let decode = |k: &str| {
            STANDARD
                .decode(field(c, k).as_str().unwrap_or_default())
                .ok()
        };
        let width = u32::try_from(field(c, "SkinImageWidth").as_u64()?).ok()?;
        let height = u32::try_from(field(c, "SkinImageHeight").as_u64()?).ok()?;
        let data = decode("SkinData")?;
        let expected = usize::try_from(width.checked_mul(height)?.checked_mul(4)?).ok()?;
        if data.len() != expected || expected > 512 * 512 * 4 {
            return None;
        }
        let slim = field(c, "ArmSize").as_str() == Some("slim");
        // Patch and geometry go to other clients as they are: the client rejects the whole
        // packet if either is not valid JSON, so anything else falls back to the built-in model.
        let custom = decode("SkinResourcePatch")
            .and_then(json_object)
            .zip(decode("SkinGeometryData").and_then(json_object));
        let (resource_patch, geometry) = custom.unwrap_or_else(|| {
            let patch = if slim { SLIM_PATCH } else { DEFAULT_PATCH };
            (patch.into(), HUMANOID_GEOMETRY.into())
        });
        Some(Self {
            id: field(c, "SkinId")
                .as_str()
                .unwrap_or("custom")
                .chars()
                .take(128)
                .collect(),
            resource_patch,
            width,
            height,
            data,
            geometry,
            geometry_version: decode("SkinGeometryDataEngineVersion").unwrap_or_default(),
            slim,
        })
    }

    /// Writes the network form.
    pub fn write(&self, out: &mut Vec<u8>, v: &crate::version::Version) {
        io::write_str(out, &self.id);
        if v.before_26_60() {
            io::write_str(out, ""); // PlayFab id
        }
        io::write_bytes(out, &self.resource_patch);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        io::write_bytes(out, &self.data);
        io::write_varu32(out, 0); // animations
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        io::write_bytes(out, &[]); // cape
        io::write_bytes(out, &self.geometry);
        io::write_bytes(out, &self.geometry_version);
        io::write_bytes(out, &[]); // animation data
        io::write_str(out, ""); // cape id
        io::write_str(out, &self.id); // full id
        out.push(if self.slim { 0 } else { 1 });
        out.extend_from_slice(&[0; 4]); // skin colour
        io::write_varu32(out, 0); // persona pieces
        io::write_varu32(out, 0); // piece tints
        out.extend_from_slice(&[0, 0, 0, 0, 0]); // premium, persona, cape on classic, primary, override
        io::write_str(out, "True"); // trusted
        io::write_str(out, ""); // profile hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client rejects `player_list` (packet 63) unless resource patch and geometry are JSON
    /// (#280: an empty geometry for Java and default skins disconnected every Bedrock player).
    fn assert_json(skin: &Skin) {
        for (what, bytes) in [
            ("patch", &skin.resource_patch),
            ("geometry", &skin.geometry),
        ] {
            let v: Value = serde_json::from_slice(bytes)
                .unwrap_or_else(|e| panic!("{} {what} is not JSON: {e}", skin.id));
            assert!(
                v.is_object() && depth(&v) <= MAX_JSON_DEPTH,
                "{} {what}",
                skin.id
            );
        }
    }

    #[test]
    fn every_skin_carries_json_patch_and_geometry() {
        let plain = Skin::plain();
        assert_json(&plain);
        assert_json(&plain.skull());
        let client = |geometry: &str| {
            serde_json::json!({
                "SkinId": "custom",
                "SkinImageWidth": 64,
                "SkinImageHeight": 64,
                "SkinData": STANDARD.encode(vec![0u8; 64 * 64 * 4]),
                "SkinResourcePatch": STANDARD.encode(SLIM_PATCH),
                "SkinGeometryData": STANDARD.encode(geometry),
                "ArmSize": "slim",
            })
        };
        let deep = format!("{}1{}", r#"{"a":"#.repeat(100), "}".repeat(100));
        for geometry in ["", "null", "{", &deep] {
            let Some(skin) = Skin::from_client_data(&client(geometry)) else {
                panic!("no skin for geometry {geometry:?}");
            };
            assert_json(&skin);
            assert_eq!(skin.geometry, HUMANOID_GEOMETRY.as_bytes(), "{geometry:?}");
            assert_eq!(skin.resource_patch, SLIM_PATCH.as_bytes());
        }
        let own = Skin::from_client_data(&client(SKULL_GEOMETRY)).map(|s| s.geometry);
        assert_eq!(own.as_deref(), Some(SKULL_GEOMETRY.as_bytes()));
    }
}
