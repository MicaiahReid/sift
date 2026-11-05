//! Local IDL discovery for Anchor and Codama projects.
//!
//! Three strategies run from the nearest Cargo workspace or package root:
//!
//! 1. `Anchor.toml` program tables name the program; the matching
//!    `target/idl/<name>.json` is used even when the IDL declares no address.
//! 2. `codama.json` configuration files point at an IDL through their `idl`
//!    key; the IDL is used when it declares the requested program address.
//! 3. Any other JSON document in the project that parses as an Anchor or
//!    Codama IDL and declares the requested program address.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use toml_edit::{DocumentMut, Item};
use tracing::{debug, trace};
use walkdir::{DirEntry, WalkDir};

use crate::idl::{IdlFormat, declared_program_address, detect_format, parse_idl};

/// IDL documents larger than this are not sniffed during the project scan.
const MAX_SCANNED_IDL_BYTES: u64 = 32 * 1024 * 1024;

/// A validated local IDL associated with a requested program address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalIdlCandidate {
    /// The program name declared by the IDL or its Anchor.toml entry.
    pub program_name: String,
    /// Project directory the IDL belongs to.
    pub member_dir: PathBuf,
    /// Existing and parseable IDL JSON document.
    pub idl_path: PathBuf,
    /// Dialect of the document.
    pub format: IdlFormat,
}

/// Finds parseable Anchor or Codama IDLs for a program from the nearest Cargo project.
///
/// The closest ancestor Cargo workspace is used when present; otherwise the
/// closest package manifest is used. Results are sorted by path and contain
/// no duplicates.
pub fn discover_local_idls(start: &Path, program_address: &str) -> Result<Vec<LocalIdlCandidate>> {
    let Some(root) = nearest_cargo_root(start)? else {
        debug!(start = %start.display(), "no Cargo project found for local IDL discovery");
        return Ok(Vec::new());
    };
    debug!(start = %start.display(), root = %root.display(), program_address, "starting local IDL discovery");
    let manifest = parse_toml(&root.join("Cargo.toml"))?;
    let members = workspace_members(&root, &manifest)?;
    let anchor_projects = anchor_project_dirs(&root, &members)?;
    debug!(
        member_count = members.len(),
        anchor_project_count = anchor_projects.len(),
        "discovered local project candidates"
    );
    let mut discovery = Discovery {
        program_address,
        candidates: Vec::new(),
        seen: BTreeSet::new(),
    };
    for member_dir in &anchor_projects {
        let anchor_toml = member_dir.join("Anchor.toml");
        if !anchor_toml.is_file() {
            trace!(path = %member_dir.display(), "skipping project without Anchor.toml");
            continue;
        }
        for program_name in anchor_programs(&anchor_toml, program_address)? {
            debug!(anchor_toml = %anchor_toml.display(), program_name, "Anchor program address matched");
            for target_dir in [member_dir.join("target/idl"), root.join("target/idl")] {
                let path = target_dir.join(format!("{program_name}.json"));
                if !path.is_file() {
                    trace!(path = %path.display(), "expected IDL file not found");
                    continue;
                }
                discovery.consider(&path, member_dir, Some(&program_name), false)?;
            }
        }
    }
    for config in codama_configs(&root)? {
        let Some(idl_path) = codama_config_idl(&config)? else {
            continue;
        };
        let member_dir = config.parent().unwrap_or(&root).to_path_buf();
        discovery.consider(&idl_path, &member_dir, None, true)?;
    }
    for path in project_json_files(&root, &members)? {
        let member_dir = members
            .iter()
            .filter(|member| path.starts_with(member))
            .max_by_key(|member| member.components().count())
            .cloned()
            .unwrap_or_else(|| root.clone());
        discovery.consider(&path, &member_dir, None, true)?;
    }
    let mut candidates = discovery.candidates;
    candidates.sort_by(|left, right| left.idl_path.cmp(&right.idl_path));
    debug!(
        candidate_count = candidates.len(),
        "completed local IDL discovery"
    );
    Ok(candidates)
}

struct Discovery<'a> {
    program_address: &'a str,
    candidates: Vec<LocalIdlCandidate>,
    seen: BTreeSet<PathBuf>,
}

impl Discovery<'_> {
    /// Validates one file and records it once.
    ///
    /// An `opportunistic` file was found by scanning rather than named by
    /// configuration: it must declare the requested program address itself,
    /// and one that cannot be read is skipped instead of failing discovery.
    /// Anchor.toml matches are not opportunistic because legacy Anchor IDLs
    /// carry no address.
    fn consider(
        &mut self,
        path: &Path,
        member_dir: &Path,
        program_name: Option<&str>,
        opportunistic: bool,
    ) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot resolve discovered IDL {}", path.display()))?;
        if self.seen.contains(&canonical) {
            trace!(path = %canonical.display(), "skipping duplicate discovered IDL");
            return Ok(());
        }
        let document = match read_idl_document(&canonical) {
            Ok(Some(document)) => document,
            Ok(None) => {
                trace!(path = %canonical.display(), "skipping non-IDL JSON");
                return Ok(());
            }
            Err(error) if opportunistic => {
                debug!(path = %canonical.display(), error = %error, "skipping unreadable file during IDL scan");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let Some(format) = detect_format(&document) else {
            return Ok(());
        };
        let declared = declared_program_address(&document);
        if opportunistic && declared.as_deref() != Some(self.program_address) {
            trace!(path = %canonical.display(), ?declared, "IDL declares a different program");
            return Ok(());
        }
        if declared
            .as_deref()
            .is_some_and(|declared| declared != self.program_address)
        {
            debug!(path = %canonical.display(), declared, "Anchor.toml IDL declares another program address; skipping");
            return Ok(());
        }
        if let Err(error) = parse_idl(&document.to_string(), self.program_address) {
            trace!(path = %canonical.display(), error = %error, "skipping unsupported discovered IDL");
            return Ok(());
        }
        let program_name = program_name
            .map(ToOwned::to_owned)
            .or_else(|| idl_program_name(&document, format))
            .unwrap_or_else(|| {
                canonical
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        debug!(path = %canonical.display(), program_name, format = format.label(), "accepted discovered local IDL");
        self.seen.insert(canonical.clone());
        self.candidates.push(LocalIdlCandidate {
            program_name,
            member_dir: member_dir.to_path_buf(),
            idl_path: canonical,
            format,
        });
        Ok(())
    }
}

/// Reads a JSON file when it plausibly holds an IDL, skipping huge or unrelated files cheaply.
fn read_idl_document(path: &Path) -> Result<Option<Value>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot inspect discovered IDL {}", path.display()))?;
    if metadata.len() > MAX_SCANNED_IDL_BYTES {
        return Ok(None);
    }
    let bytes =
        fs::read(path).with_context(|| format!("cannot read discovered IDL {}", path.display()))?;
    // JSON must be UTF-8, so anything else cannot be an IDL.
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    // Both dialects contain one of these keys; package manifests and lockfiles do not.
    if !(text.contains("\"rootNode\"")
        || text.contains("\"programNode\"")
        || text.contains("\"instructions\"")
        || text.contains("\"accounts\""))
    {
        return Ok(None);
    }
    Ok(serde_json::from_str(text).ok())
}

fn idl_program_name(document: &Value, format: IdlFormat) -> Option<String> {
    match format {
        IdlFormat::Codama => document
            .pointer("/program/name")
            .or_else(|| document.get("name")),
        IdlFormat::Anchor => document
            .pointer("/metadata/name")
            .or_else(|| document.get("name")),
    }
    .and_then(Value::as_str)
    .map(ToOwned::to_owned)
}

/// Finds every `codama.json` configuration in the project.
fn codama_configs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut configs = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(should_visit) {
        let entry =
            entry.with_context(|| format!("cannot scan local workspace {}", root.display()))?;
        if entry.file_type().is_file() && entry.file_name() == "codama.json" {
            configs.push(entry.into_path());
        }
    }
    Ok(configs)
}

/// Resolves the `idl` path named by a Codama configuration file.
fn codama_config_idl(config: &Path) -> Result<Option<PathBuf>> {
    let text =
        fs::read_to_string(config).with_context(|| format!("cannot read {}", config.display()))?;
    let Ok(document) = serde_json::from_str::<Value>(&text) else {
        debug!(path = %config.display(), "codama.json is not valid JSON");
        return Ok(None);
    };
    let Some(idl) = document.get("idl").and_then(Value::as_str) else {
        return Ok(None);
    };
    let base = config.parent().unwrap_or(Path::new("."));
    Ok(Some(base.join(idl)))
}

/// Lists JSON files worth sniffing for an IDL: the project tree minus build
/// and dependency directories, plus the conventional `idl` output directories.
fn project_json_files(root: &Path, members: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(should_visit) {
        let entry =
            entry.with_context(|| format!("cannot scan local workspace {}", root.display()))?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            files.insert(entry.into_path());
        }
    }
    for directory in members
        .iter()
        .map(PathBuf::as_path)
        .chain(std::iter::once(root))
        .flat_map(|member| [member.join("target/idl"), member.join("idl")])
    {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                files.insert(path);
            }
        }
    }
    Ok(files.into_iter().collect())
}

fn anchor_project_dirs(root: &Path, members: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut projects = members.iter().cloned().collect::<BTreeSet<_>>();
    for entry in WalkDir::new(root).into_iter().filter_entry(should_visit) {
        let entry =
            entry.with_context(|| format!("cannot scan local workspace {}", root.display()))?;
        if !entry.file_type().is_file() || entry.file_name() != "Anchor.toml" {
            continue;
        }
        let parent = entry
            .path()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Anchor.toml has no parent"))?;
        projects.insert(
            parent
                .canonicalize()
                .with_context(|| format!("cannot resolve Anchor project {}", parent.display()))?,
        );
    }
    Ok(projects.into_iter().collect())
}

fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | "target" | "node_modules" | "vendor" | "dist" | ".next")
    )
}

fn nearest_cargo_root(start: &Path) -> Result<Option<PathBuf>> {
    let start = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent().unwrap_or(start).to_path_buf()
    };
    let mut package_root = None;
    for directory in start.ancestors() {
        let manifest = directory.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let directory = directory
            .canonicalize()
            .with_context(|| format!("cannot resolve Cargo workspace {}", directory.display()))?;
        let document = parse_toml(&directory.join("Cargo.toml"))?;
        if document.get("workspace").is_some() {
            return Ok(Some(directory));
        }
        if package_root.is_none() {
            package_root = Some(directory);
        }
    }
    Ok(package_root)
}

fn workspace_members(root: &Path, manifest: &DocumentMut) -> Result<Vec<PathBuf>> {
    let mut members = BTreeSet::new();
    // Anchor projects commonly put Anchor.toml beside a virtual Cargo workspace.
    // Inspecting the root unconditionally covers that layout as well as package roots.
    members.insert(root.to_path_buf());
    let Some(workspace) = manifest.get("workspace").and_then(Item::as_table_like) else {
        return Ok(members.into_iter().collect());
    };
    let excludes = workspace
        .get("exclude")
        .and_then(Item::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(ToOwned::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    for pattern in workspace
        .get("members")
        .and_then(Item::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
    {
        let absolute_pattern = root.join(pattern).to_string_lossy().to_string();
        for entry in glob::glob(&absolute_pattern)
            .with_context(|| format!("invalid workspace member glob {pattern}"))?
            .flatten()
        {
            let member_dir = if entry.file_name().is_some_and(|name| name == "Cargo.toml") {
                entry.parent().unwrap_or(&entry).to_path_buf()
            } else {
                entry
            };
            let relative = member_dir
                .strip_prefix(root)
                .ok()
                .map(|path| path.to_string_lossy().to_string());
            if relative
                .as_ref()
                .is_some_and(|path| excludes.contains(path))
                || !member_dir.join("Cargo.toml").is_file()
            {
                continue;
            }
            members.insert(member_dir.canonicalize().with_context(|| {
                format!("cannot resolve workspace member {}", member_dir.display())
            })?);
        }
    }
    Ok(members.into_iter().collect())
}

fn anchor_programs(path: &Path, program_address: &str) -> Result<Vec<String>> {
    let document = parse_toml(path)?;
    let mut names = BTreeSet::new();
    let Some(clusters) = document.get("programs").and_then(Item::as_table_like) else {
        return Ok(Vec::new());
    };
    for (_, cluster) in clusters.iter() {
        let Some(programs) = cluster.as_table_like() else {
            continue;
        };
        for (name, address) in programs.iter() {
            if address.as_str() == Some(program_address) {
                names.insert(name.to_owned());
            }
        }
    }
    Ok(names.into_iter().collect())
}

fn parse_toml(path: &Path) -> Result<DocumentMut> {
    fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .parse::<DocumentMut>()
        .with_context(|| format!("invalid TOML in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};
    use tempfile::TempDir;

    const ADDRESS: &str = "11111111111111111111111111111111";
    const OTHER: &str = "22222222222222222222222222222222";

    fn codama_idl(address: &str) -> String {
        format!(
            r#"{{"kind":"rootNode","standard":"codama","version":"1.0.0","program":{{"kind":"programNode","name":"demo","publicKey":"{address}","version":"0.1.0","accounts":[],"instructions":[{{"kind":"instructionNode","name":"ping","accounts":[],"arguments":[]}}],"definedTypes":[],"pdas":[],"errors":[]}}}}"#
        )
    }

    #[test]
    fn finds_member_target_idl_before_workspace_target() {
        let temp = TempDir::new().unwrap();
        write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"programs/*\"]\n",
        )
        .unwrap();
        let member = temp.path().join("programs/demo");
        create_dir_all(member.join("target/idl")).unwrap();
        write(
            member.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        write(
            member.join("Anchor.toml"),
            format!("[programs.localnet]\ndemo = \"{ADDRESS}\"\n"),
        )
        .unwrap();
        write(
            member.join("target/idl/demo.json"),
            "{\"name\":\"demo\",\"accounts\":[],\"instructions\":[{\"name\":\"ping\",\"accounts\":[],\"args\":[]}]}",
        )
        .unwrap();
        let candidates = discover_local_idls(&member, ADDRESS).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].idl_path,
            member.join("target/idl/demo.json").canonicalize().unwrap()
        );
        assert_eq!(candidates[0].format, IdlFormat::Anchor);
    }

    #[test]
    fn falls_back_to_workspace_target() {
        let temp = TempDir::new().unwrap();
        write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"demo\"]\n",
        )
        .unwrap();
        let member = temp.path().join("demo");
        create_dir_all(temp.path().join("target/idl")).unwrap();
        create_dir_all(&member).unwrap();
        write(
            member.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        write(
            member.join("Anchor.toml"),
            format!("[programs.devnet]\ndemo = \"{ADDRESS}\"\n"),
        )
        .unwrap();
        write(temp.path().join("target/idl/demo.json"), "{\"name\":\"demo\",\"accounts\":[],\"instructions\":[{\"name\":\"ping\",\"accounts\":[],\"args\":[]}]}").unwrap();
        assert_eq!(discover_local_idls(&member, ADDRESS).unwrap().len(), 1);
    }

    #[test]
    fn discovers_anchor_toml_at_a_virtual_workspace_root() {
        let temp = TempDir::new().unwrap();
        write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"client\"]\n",
        )
        .unwrap();
        write(
            temp.path().join("Anchor.toml"),
            format!("[programs.localnet]\ndemo = \"{ADDRESS}\"\n"),
        )
        .unwrap();
        create_dir_all(temp.path().join("client")).unwrap();
        write(
            temp.path().join("client/Cargo.toml"),
            "[package]\nname = \"client\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        create_dir_all(temp.path().join("target/idl")).unwrap();
        write(temp.path().join("target/idl/demo.json"), "{\"name\":\"demo\",\"accounts\":[],\"instructions\":[{\"name\":\"ping\",\"accounts\":[],\"args\":[]}]}").unwrap();
        assert_eq!(discover_local_idls(temp.path(), ADDRESS).unwrap().len(), 1);
    }

    #[test]
    fn discovers_anchor_project_outside_workspace_members() {
        let temp = TempDir::new().unwrap();
        write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"programs/demo\"]\n",
        )
        .unwrap();
        let anchor_project = temp.path().join("ncn");
        create_dir_all(anchor_project.join("target/idl")).unwrap();
        write(
            anchor_project.join("Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .unwrap();
        write(
            anchor_project.join("Anchor.toml"),
            format!("[programs.localnet]\ndemo = \"{ADDRESS}\"\n"),
        )
        .unwrap();
        write(anchor_project.join("target/idl/demo.json"), "{\"name\":\"demo\",\"accounts\":[],\"instructions\":[{\"name\":\"ping\",\"accounts\":[],\"args\":[]}]}").unwrap();
        create_dir_all(temp.path().join("programs/demo")).unwrap();
        write(
            temp.path().join("programs/demo/Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(discover_local_idls(temp.path(), ADDRESS).unwrap().len(), 1);
    }

    #[test]
    fn discovers_codama_idls_through_config_and_declared_address() {
        let temp = TempDir::new().unwrap();
        write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"program\"]\n",
        )
        .unwrap();
        create_dir_all(temp.path().join("program/idl")).unwrap();
        write(
            temp.path().join("program/Cargo.toml"),
            "[package]\nname = \"program\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        // Referenced by codama.json, outside the scanned tree.
        create_dir_all(temp.path().join("target/generated")).unwrap();
        write(
            temp.path().join("target/generated/idl.json"),
            codama_idl(ADDRESS),
        )
        .unwrap();
        write(
            temp.path().join("codama.json"),
            r#"{"idl": "target/generated/idl.json", "scripts": {}}"#,
        )
        .unwrap();
        // Found by scanning; declares the requested program.
        write(
            temp.path().join("program/idl/program.codama.json"),
            codama_idl(ADDRESS),
        )
        .unwrap();
        // Declares another program and must be ignored.
        write(
            temp.path().join("program/idl/other.json"),
            codama_idl(OTHER),
        )
        .unwrap();
        // Unrelated JSON must not be reported.
        write(
            temp.path().join("package.json"),
            r#"{"name": "x", "instructions": "none"}"#,
        )
        .unwrap();
        // A binary fixture with a .json name must not abort the scan.
        write(temp.path().join("program/fixture.json"), [0xff, 0xfe, b'{']).unwrap();

        let candidates = discover_local_idls(&temp.path().join("program"), ADDRESS).unwrap();
        let paths = candidates
            .iter()
            .map(|candidate| candidate.idl_path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                temp.path()
                    .join("program/idl/program.codama.json")
                    .canonicalize()
                    .unwrap(),
                temp.path()
                    .join("target/generated/idl.json")
                    .canonicalize()
                    .unwrap(),
            ]
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.format == IdlFormat::Codama)
        );
        assert_eq!(candidates[0].program_name, "demo");
        assert_eq!(
            candidates[0].member_dir,
            temp.path().join("program").canonicalize().unwrap()
        );
    }
}
