//! Local Cargo-workspace and Anchor project IDL discovery.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item};
use tracing::{debug, trace};
use walkdir::{DirEntry, WalkDir};

use crate::idl::parse_idl;

/// A validated local Anchor IDL associated with a requested program address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalIdlCandidate {
    /// The Anchor program name used as the IDL filename stem.
    pub program_name: String,
    /// Cargo workspace member that declared the matching program address.
    pub member_dir: PathBuf,
    /// Existing and parseable IDL JSON document.
    pub idl_path: PathBuf,
}

/// Finds parseable Anchor IDLs for a program from the nearest Cargo workspace.
///
/// The closest ancestor Cargo workspace is used when present; otherwise the
/// closest package manifest is used. Its package and workspace members are inspected for
/// `Anchor.toml`; each matching program is looked up in member then workspace
/// `target/idl` directories.
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
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for member_dir in anchor_projects {
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
                let canonical = path
                    .canonicalize()
                    .with_context(|| format!("cannot resolve discovered IDL {}", path.display()))?;
                if !seen.insert(canonical.clone()) {
                    trace!(path = %canonical.display(), "skipping duplicate discovered IDL");
                    continue;
                }
                let text = fs::read_to_string(&canonical).with_context(|| {
                    format!("cannot read discovered IDL {}", canonical.display())
                })?;
                if parse_idl(&text, program_address).is_ok() {
                    debug!(path = %canonical.display(), program_name, "accepted discovered local IDL");
                    candidates.push(LocalIdlCandidate {
                        program_name: program_name.clone(),
                        member_dir: member_dir.clone(),
                        idl_path: canonical,
                    });
                } else {
                    trace!(path = %canonical.display(), "skipping unsupported discovered IDL");
                }
            }
        }
    }
    candidates.sort_by(|left, right| left.idl_path.cmp(&right.idl_path));
    debug!(
        candidate_count = candidates.len(),
        "completed local IDL discovery"
    );
    Ok(candidates)
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
        Some(".git" | "target" | "node_modules" | "vendor")
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
}
