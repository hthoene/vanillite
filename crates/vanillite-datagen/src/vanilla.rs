//! Server jars: download with SHA-1 check, Mojang's data generator, the remapper for obfuscated
//! jars, our extractor (`tools/extractor`), and the JDK they run on.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::Result;
use crate::normalize::VersionInfo;

const MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

/// Quiet mode (build scripts): the output of Java and the progress lines are not printed; a
/// failing tool's output goes into the error instead.
static QUIET: AtomicBool = AtomicBool::new(false);

/// Turns quiet mode on (see [`QUIET`]).
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

/// Prints a progress line unless quiet.
pub fn progress(line: &str) {
    if !QUIET.load(Ordering::Relaxed) {
        println!("{line}");
    }
}

/// Mojang's version manifest.
pub fn manifest() -> Result<Value> {
    http_json(MANIFEST)
}

/// A version's server jar run through Mojang's data generator and our extractor, in
/// `<work root>/<id>/`.
#[derive(Debug)]
pub struct Extracted {
    /// `release`, `snapshot`, ... from the manifest.
    pub kind: String,
    pub release_time: String,
    pub work: PathBuf,
    /// The unbundled server jar (classes and the built-in data pack).
    pub inner_jar: PathBuf,
    /// Datagen output (`reports/`).
    pub generated: PathBuf,
    /// Extractor output.
    pub extracted: PathBuf,
    /// Whether Mojang published mappings (none since the jars are unobfuscated).
    pub mappings: bool,
    pub info: VersionInfo,
}

/// Which extractor runs against a jar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extractor {
    /// Everything `data/vanilla/` needs (`Main`, needs the current API).
    Full,
    /// What `data/multiversion/` needs (`MultiversionMain`, runs against every version from
    /// 1.20.5 on; obfuscated jars are remapped with Mojang's mappings first).
    Multiversion,
}

/// Downloads (or takes) the server jar of `version` (default: the latest release) into
/// `<work_root>/<id>/`, verifies its SHA-1, fetches the mappings if there are any, and runs
/// datagen and the extractor (sources from `<root>/tools/extractor`).
pub fn extract(
    root: &Path,
    work_root: &Path,
    manifest: &Value,
    version: Option<&str>,
    local_jar: Option<&Path>,
    extractor: Extractor,
) -> Result<Extracted> {
    let id = match version {
        Some(v) => v.to_owned(),
        None => str_at(manifest, &["latest", "release"])?.to_owned(),
    };
    let entry = manifest["versions"]
        .as_array()
        .and_then(|vs| vs.iter().find(|v| v["id"] == id.as_str()))
        .ok_or_else(|| format!("version {id} is not in the version manifest"))?;
    let meta = http_json(str_at(entry, &["url"])?)?;
    let jar_url = str_at(&meta, &["downloads", "server", "url"])?.to_owned();
    let jar_sha1 = str_at(&meta, &["downloads", "server", "sha1"])?.to_owned();
    let java_major = meta["javaVersion"]["majorVersion"].as_u64().unwrap_or(0);
    progress(&format!(
        "vanilla: extracting {id} (server.jar {jar_sha1}, Java {java_major})"
    ));
    let java_home = java_home(java_major, &crate::cache::cache_root(root))?;
    let java = |name: &str| match &java_home {
        Some(home) => home.join("bin").join(exe(name)),
        None => PathBuf::from(name),
    };

    let work = work_root.join(&id);
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let jar = work.join("server.jar");
    if let Some(local) = local_jar {
        std::fs::copy(local, &jar).map_err(|e| format!("{}: {e}", local.display()))?;
    } else if !jar.exists() || sha1_file(&jar)? != jar_sha1 {
        progress(&format!("downloading {jar_url}"));
        download(&jar_url, &jar)?;
    }
    let actual = sha1_file(&jar)?;
    if actual != jar_sha1 {
        return Err(format!(
            "server.jar SHA-1 mismatch: expected {jar_sha1}, got {actual}"
        ));
    }
    let mappings = match meta["downloads"]["server_mappings"]["url"].as_str() {
        Some(url) => {
            let file = work.join("server_mappings.txt");
            if !file.exists() {
                progress(&format!("downloading {url}"));
                download(url, &file)?;
            }
            true
        }
        None => false,
    };

    // Mojang's data generator. The bundler also unpacks the inner jar and its libraries.
    let generated = work.join("generated");
    remove_dir(&generated)?;
    run(Command::new(java("java"))
        .current_dir(&work)
        .arg("-DbundlerMainClass=net.minecraft.data.Main")
        .args(["-jar", "server.jar", "--reports", "--output", "generated"]))
    .map_err(|e| format!("{e}\n(datagen needs Java {java_major})"))?;

    let inner = work
        .join("versions")
        .join(&id)
        .join(format!("server-{id}.jar"));
    // Obfuscated jars (before 26.1) get Mojang's names, which the extractor is written against.
    let named = if mappings && extractor == Extractor::Multiversion {
        let named = work.join(format!("server-{id}-named.jar"));
        remap(
            root,
            &work_root.join("_remapper"),
            &java,
            &inner,
            &work.join("server_mappings.txt"),
            &named,
        )?;
        named
    } else {
        inner.clone()
    };
    let mut classpath = vec![named];
    collect_jars(&work.join("libraries"), &mut classpath)?;
    classpath.sort();
    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = classpath
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(sep);

    // Our extractor, compiled against exactly this jar. The golden harness is left out: it uses
    // much more of the game's API and must not stop the data of a new version.
    let classes = work.join("extractor-classes");
    remove_dir(&classes)?;
    let sources = extractor_sources(root, extractor)?;
    let main = match extractor {
        Extractor::Full => "dev.vanillite.extractor.Main",
        Extractor::Multiversion => "dev.vanillite.extractor.MultiversionMain",
    };
    run(Command::new(java("javac"))
        .args(["-encoding", "UTF-8", "-nowarn", "-d"])
        .arg(&classes)
        .arg("-cp")
        .arg(&cp)
        .args(&sources))?;
    let extracted = work.join("extracted");
    remove_dir(&extracted)?;
    run(Command::new(java("java"))
        .current_dir(&work)
        .arg("-cp")
        .arg(format!("{}{sep}{cp}", classes.display()))
        .arg(main)
        .arg(&extracted))?;

    let version_json: Value = serde_json::from_slice(&zip_entry(&inner, "version.json")?)
        .map_err(|e| format!("version.json: {e}"))?;
    let info = VersionInfo::new(&version_json, jar_url, jar_sha1)?;
    Ok(Extracted {
        kind: entry["type"].as_str().unwrap_or_default().to_owned(),
        release_time: entry["releaseTime"].as_str().unwrap_or_default().to_owned(),
        work,
        inner_jar: inner,
        generated,
        extracted,
        mappings,
        info,
    })
}

/// The extractor sources compiled for `extractor`, sorted. The golden harness is left out: it
/// uses much more of the game's API and must not stop the data of a new version.
pub fn extractor_sources(root: &Path, extractor: Extractor) -> Result<Vec<PathBuf>> {
    let mut sources = Vec::new();
    collect_files(&root.join("tools/extractor/src"), "java", &mut sources)?;
    sources.retain(|p| !p.components().any(|c| c.as_os_str() == "golden"));
    if extractor == Extractor::Multiversion {
        sources.retain(|p| p.ends_with("MultiversionMain.java") || p.ends_with("Packets.java"));
    }
    sources.sort();
    Ok(sources)
}

/// ASM (BSD-3-Clause) for the remapper, from Maven Central: artifact, SHA-1.
const ASM: &[(&str, &str)] = &[
    ("asm", "2ceea6ab43bcae1979b2a6d85fc0ca429877e5ab"),
    ("asm-commons", "ab35de4c537184a09339069f1a3b3aacf2289149"),
    ("asm-tree", "b6b1b3366296163b4b1f540731aad0a2baa484d8"),
];
const ASM_VERSION: &str = "9.9.1";

/// Applies Mojang's mappings to an obfuscated jar (`tools/remapper/`). The result is kept.
fn remap(
    root: &Path,
    tools: &Path,
    java: &dyn Fn(&str) -> PathBuf,
    jar: &Path,
    mappings: &Path,
    out: &Path,
) -> Result<()> {
    if out.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(tools).map_err(|e| format!("{}: {e}", tools.display()))?;
    let mut classpath = Vec::new();
    for (artifact, sha1) in ASM {
        let file = tools.join(format!("{artifact}-{ASM_VERSION}.jar"));
        if !file.exists() || sha1_file(&file)? != *sha1 {
            download(
                &format!(
                    "https://repo1.maven.org/maven2/org/ow2/asm/{artifact}/{ASM_VERSION}/{artifact}-{ASM_VERSION}.jar"
                ),
                &file,
            )?;
            if sha1_file(&file)? != *sha1 {
                return Err(format!("{}: SHA-1 mismatch", file.display()));
            }
        }
        classpath.push(file.display().to_string());
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    let classes = tools.join("classes");
    remove_dir(&classes)?;
    run(Command::new(java("javac"))
        .args(["-encoding", "UTF-8", "-nowarn", "-d"])
        .arg(&classes)
        .arg("-cp")
        .arg(classpath.join(sep))
        .arg(root.join("tools/remapper/Remap.java")))?;
    classpath.push(classes.display().to_string());
    let tmp = out.with_extension("part");
    run(Command::new(java("java"))
        .arg("-cp")
        .arg(classpath.join(sep))
        .arg("dev.vanillite.remapper.Remap")
        .arg(jar)
        .arg(mappings)
        .arg(&tmp))?;
    std::fs::rename(&tmp, out).map_err(|e| format!("{}: {e}", out.display()))
}

/// `VANILLITE_JAVA_HOME`, then `JAVA_HOME`, then the tool on `PATH`.
pub fn java_tool(name: &str) -> PathBuf {
    for var in ["VANILLITE_JAVA_HOME", "JAVA_HOME"] {
        if let Some(home) = std::env::var_os(var) {
            let tool = PathBuf::from(home).join("bin").join(exe(name));
            if tool.exists() {
                return tool;
            }
        }
    }
    PathBuf::from(name)
}

/// `name` with the platform's executable suffix.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// A JDK of at least Java `major`: `VANILLITE_JAVA_HOME`, `JAVA_HOME`, GitHub's
/// `JAVA_HOME_<major>_X64`/`_ARM64`, the `java` on `PATH` (`None`), a JDK provisioned earlier in
/// `<cache>/jdk/<major>/`, or else Eclipse Temurin downloaded there from Adoptium (SHA-256
/// checked), so a fresh machine needs no manual setup.
pub fn java_home(major: u64, cache: &Path) -> Result<Option<PathBuf>> {
    let suffix = if cfg!(target_arch = "aarch64") {
        "ARM64"
    } else {
        "X64"
    };
    let vars = [
        "VANILLITE_JAVA_HOME".to_owned(),
        "JAVA_HOME".to_owned(),
        format!("JAVA_HOME_{major}_{suffix}"),
    ];
    for var in &vars {
        if let Some(home) = std::env::var_os(var).map(PathBuf::from)
            && is_jdk(&home, major)
        {
            return Ok(Some(home));
        }
    }
    if java_major(Path::new("java")).is_some_and(|m| m >= major)
        && Command::new("javac").arg("-version").output().is_ok()
    {
        return Ok(None);
    }
    // One provisioned JDK serves every version: Java runs the bytecode of older releases.
    let want = major.max(PROVISIONED_JDK);
    let jdks = cache.join("jdk");
    let dir = jdks.join(want.to_string());
    if let Some(home) = find_jdk(&dir, major) {
        return Ok(Some(home));
    }
    std::fs::create_dir_all(&jdks).map_err(|e| format!("{}: {e}", jdks.display()))?;
    let lock_path = jdks.join(format!("{want}.lock"));
    let lock =
        std::fs::File::create(&lock_path).map_err(|e| format!("{}: {e}", lock_path.display()))?;
    lock.lock()
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    if find_jdk(&dir, major).is_none() {
        provision_jdk(want, &dir)?;
    }
    find_jdk(&dir, major)
        .map(Some)
        .ok_or_else(|| format!("{}: no JDK {want} after the download", dir.display()))
}

/// The Java version provisioned when the machine has no suitable JDK (at least the one the
/// jar asks for).
const PROVISIONED_JDK: u64 = 25;

/// Whether `home` is a JDK (java and javac) of at least Java `major`.
fn is_jdk(home: &Path, major: u64) -> bool {
    home.join("bin").join(exe("javac")).exists()
        && java_major(&home.join("bin").join(exe("java"))).is_some_and(|m| m >= major)
}

/// The JDK unpacked in `dir` (`<dir>/<jdk>/` or, on macOS, `<dir>/<jdk>/Contents/Home/`).
fn find_jdk(dir: &Path, major: u64) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .flat_map(|p| [p.clone(), p.join("Contents").join("Home")])
        .find(|home| is_jdk(home, major))
}

/// The major version `java -version` reports (`"25.0.1"` → 25, `"1.8.0_402"` → 8).
fn java_major(java: &Path) -> Option<u64> {
    let out = Command::new(java).arg("-version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let version = text.split('"').nth(1)?;
    let mut parts = version.split(['.', '_', '-', '+']);
    let first: u64 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// Downloads Eclipse Temurin `major` (JDK) for this machine from Adoptium into `dir`.
fn provision_jdk(major: u64, dir: &Path) -> Result<()> {
    let os = match std::env::consts::OS {
        "macos" => "mac",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        arch => arch,
    };
    let api = format!(
        "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?architecture={arch}&image_type=jdk&os={os}&vendor=eclipse"
    );
    let assets = http_json(&api)?;
    let package = &assets[0]["binary"]["package"];
    let link = str_at(package, &["link"])?;
    let sha256 = str_at(package, &["checksum"])?;
    let name = str_at(package, &["name"])?;
    progress(&format!("downloading JDK {major}: {link}"));
    remove_dir(dir)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let archive = dir.join(name);
    download(link, &archive)?;
    let actual = sha256_file(&archive)?;
    if actual != sha256 {
        return Err(format!(
            "{name}: SHA-256 mismatch: expected {sha256}, got {actual}"
        ));
    }
    // tar unpacks .tar.gz everywhere and .zip on Windows (bsdtar).
    run(Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(dir))?;
    std::fs::remove_file(&archive).map_err(|e| format!("{}: {e}", archive.display()))
}

/// Runs a tool; quiet, its output goes into the error if it fails.
pub fn run(cmd: &mut Command) -> Result<()> {
    if QUIET.load(Ordering::Relaxed) {
        let out = cmd
            .output()
            .map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
        if out.status.success() {
            return Ok(());
        }
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let lines: Vec<&str> = text.lines().collect();
        let tail = lines[lines.len().saturating_sub(60)..].join("\n");
        return Err(format!(
            "{:?} failed: {}\n{tail}",
            cmd.get_program(),
            out.status
        ));
    }
    let status = cmd
        .status()
        .map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed: {status}", cmd.get_program()))
    }
}

pub fn str_at<'a>(v: &'a Value, path: &[&str]) -> Result<&'a str> {
    path.iter()
        .fold(v, |v, k| &v[*k])
        .as_str()
        .ok_or_else(|| format!("missing {} in Mojang metadata", path.join(".")))
}

pub fn http_json(url: &str) -> Result<Value> {
    let body = retry(url, || curl(url, None))?;
    serde_json::from_slice(&body).map_err(|e| format!("{url}: {e}"))
}

pub fn download(url: &str, to: &Path) -> Result<()> {
    let tmp = to.with_extension("part");
    retry(url, || curl(url, Some(&tmp)))?;
    std::fs::rename(&tmp, to).map_err(|e| format!("{}: {e}", to.display()))
}

/// Fetches `url` (following redirects) with the system `curl`, into `to` or returned. An HTTP
/// client crate here would be compiled a second time for every build script that uses this
/// crate, at the start of every cold build.
fn curl(url: &str, to: Option<&Path>) -> Result<Vec<u8>> {
    let mut cmd = Command::new("curl");
    cmd.args(["--fail", "--silent", "--show-error", "--location"]);
    if let Some(to) = to {
        cmd.arg("--output").arg(to);
    }
    let out = cmd
        .arg(url)
        .output()
        .map_err(|e| format!("curl (needed to download {url}): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{url}: curl failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Runs a network request up to four times (1, 2, 4 s apart): one dropped connection must not
/// fail a build.
fn retry<T>(url: &str, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut wait = std::time::Duration::from_secs(1);
    for _ in 0..3 {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                progress(&format!("{e}; retrying {url} in {wait:?}"));
                std::thread::sleep(wait);
                wait *= 2;
            }
        }
    }
    f()
}

pub fn sha1_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Sha256;
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub fn zip_entry(jar: &Path, name: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(jar).map_err(|e| format!("{}: {e}", jar.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("{}: {e}", jar.display()))?;
    let mut entry = zip
        .by_name(name)
        .map_err(|e| format!("{}!{name}: {e}", jar.display()))?;
    let mut buf = Vec::new();
    entry
        .read_to_end(&mut buf)
        .map_err(|e| format!("{}!{name}: {e}", jar.display()))?;
    Ok(buf)
}

pub fn remove_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}

pub fn collect_jars(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    collect_files(dir, "jar", out)
}

pub fn collect_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            collect_files(&path, ext, out)?;
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
    Ok(())
}
