//! Shared CLI parsing, JSON input, and byte rendering helpers.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use clap::ValueEnum;
use dialoguer::{Confirm, Select, console::style, theme::ColorfulTheme};
use directories::BaseDirs;
use serde_json::{Value, json};
use tracing::{debug, warn};
use wip_core::{LocalIdlCandidate, Registry, discover_local_idls, parse_cli_value};
use wip_types::{IdlSource, ProgramSchema};

use crate::ContextOptions;

/// Output encoding for account bytes.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum BytesFormat {
    /// Standard padded base64.
    Base64,
    /// Solana's usual base58 representation.
    Base58,
    /// Lowercase hexadecimal.
    Hex,
    /// Raw bytes written to standard output.
    Raw,
    /// JSON containing decoded data and base64 bytes.
    Breakdown,
}

/// Parses a repeatable `name=value` CLI option.
pub(crate) fn parse_assignment(value: &str) -> Result<(String, Value), String> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| "use name=value".to_owned())?;
    if key.is_empty() {
        return Err("assignment name cannot be empty".to_owned());
    }
    Ok((key.to_owned(), parse_cli_value(value)))
}

/// Converts repeatable values into a last-write-wins lookup map.
pub(crate) fn value_map(items: Vec<(String, Value)>) -> BTreeMap<String, Value> {
    items.into_iter().collect()
}

/// Validates account assignment values are strings, then creates a lookup map.
pub(crate) fn account_map(items: Vec<(String, Value)>) -> Result<BTreeMap<String, String>> {
    items
        .into_iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or_else(|| anyhow::anyhow!("account {key} must be a base58 string"))
        })
        .collect()
}

/// Reads a literal JSON value, an `@path` file, or standard input (`-`).
pub(crate) async fn read_text(value: Option<String>) -> Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(path) if path == "-" => {
            use std::io::Read;
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            Ok(Some(text))
        }
        Some(path) if path.starts_with('@') => {
            Ok(Some(tokio::fs::read_to_string(&path[1..]).await?))
        }
        Some(value) => Ok(Some(value)),
    }
}

/// Reads an optional JSON object whose values must be base58 strings.
pub(crate) async fn read_account_json(value: Option<String>) -> Result<BTreeMap<String, String>> {
    match read_text(value).await? {
        None => Ok(BTreeMap::new()),
        Some(value) => serde_json::from_str(&value).context("--accounts-json must be a string map"),
    }
}

/// Reads an optional JSON object holding dynamic IDL values.
pub(crate) async fn read_value_json(
    value: Option<String>,
    flag: &str,
) -> Result<BTreeMap<String, Value>> {
    match read_text(value).await? {
        None => Ok(BTreeMap::new()),
        Some(value) => {
            serde_json::from_str(&value).with_context(|| format!("{flag} must be an object"))
        }
    }
}

/// Loads a program schema, discovering a local Anchor IDL only after normal resolution fails.
pub(crate) async fn resolve_schema(
    registry: &mut Registry,
    program: &str,
    context: &ContextOptions,
) -> Result<ProgramSchema> {
    Ok(resolve_schema_with_source(registry, program, context)
        .await?
        .schema)
}

/// A resolved schema and the actual IDL location used to produce it.
pub(crate) struct ResolvedSchema {
    pub(crate) schema: ProgramSchema,
    pub(crate) idl_location: String,
}

/// Loads a program schema and retains the source location for interactive presentation.
pub(crate) async fn resolve_schema_with_source(
    registry: &mut Registry,
    program: &str,
    context: &ContextOptions,
) -> Result<ResolvedSchema> {
    debug!(program, explicit_idl = ?context.idl, "attempting schema resolution");
    match registry
        .load_schema_with_source(program, context.idl.as_deref())
        .await
    {
        Ok(loaded) => {
            debug!(program, schema = %loaded.schema.name, source = ?loaded.source, "resolved schema from configured source");
            let idl_location = idl_source_location(&loaded.source);
            Ok(ResolvedSchema {
                schema: loaded.schema,
                idl_location,
            })
        }
        Err(error) if context.idl.is_some() => Err(error),
        Err(error) => {
            debug!(program, error = %error, "configured schema resolution failed; trying local discovery");
            let initial_error = error;
            let address = registry.program_address(program)?;
            let current_dir =
                std::env::current_dir().context("cannot determine current directory")?;
            let candidates = discover_local_idls(&current_dir, &address)?;
            if candidates.is_empty() {
                warn!(
                    program,
                    address, "local IDL discovery found no matching candidates"
                );
                Err(initial_error.context("no matching local Anchor IDL was discovered"))
            } else {
                debug!(
                    program,
                    candidate_count = candidates.len(),
                    "local IDL discovery found candidates"
                );
                let selected = select_candidate(&candidates, context.no_input)?;
                debug!(path = %selected.idl_path.display(), program_name = %selected.program_name, "selected discovered local IDL");
                if context.no_input || !std::io::stdin().is_terminal() {
                    print_status(&format!(
                        "Using discovered IDL temporarily: {}",
                        display_path(&selected.idl_path)
                    ));
                    let loaded = registry
                        .load_schema_with_source(program, Some(&selected.idl_path))
                        .await?;
                    let idl_location = idl_source_location(&loaded.source);
                    Ok(ResolvedSchema {
                        schema: loaded.schema,
                        idl_location,
                    })
                } else if !confirm_registration(&selected)? {
                    debug!(path = %selected.idl_path.display(), "user declined discovered IDL registration");
                    Err(initial_error.context(format!(
                        "discovered local IDL {} was not registered",
                        display_path(&selected.idl_path)
                    )))
                } else {
                    registry.add_source_with_aliases(
                        &address,
                        IdlSource::File {
                            name: file_source_name(&selected.idl_path),
                            path: selected.idl_path.clone(),
                        },
                        anchor_program_aliases(&selected.program_name),
                    )?;
                    registry.save().await?;
                    debug!(path = %selected.idl_path.display(), aliases = ?anchor_program_aliases(&selected.program_name), "persisted discovered IDL");
                    print_status(&format!(
                        "Registered discovered IDL: {}",
                        display_path(&selected.idl_path)
                    ));
                    let loaded = registry.load_schema_with_source(program, None).await?;
                    let idl_location = idl_source_location(&loaded.source);
                    Ok(ResolvedSchema {
                        schema: loaded.schema,
                        idl_location,
                    })
                }
            }
        }
    }
}

fn file_source_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Formats the actual selected IDL source for compact terminal presentation.
fn idl_source_location(source: &IdlSource) -> String {
    match source {
        IdlSource::File { path, .. } => display_path(path),
        IdlSource::Url { url, .. } => url.to_string(),
        IdlSource::OnChain { address, .. } => address.as_deref().map_or_else(
            || "on-chain IDL".to_owned(),
            |address| format!("on-chain IDL ({address})"),
        ),
    }
}

/// Returns the Anchor program key in its common snake- and kebab-case forms.
fn anchor_program_aliases(program_name: &str) -> Vec<String> {
    let snake_case = program_name.to_owned();
    let kebab_case = program_name.replace('_', "-");
    if snake_case == kebab_case {
        vec![snake_case]
    } else {
        vec![snake_case, kebab_case]
    }
}

fn select_candidate(candidates: &[LocalIdlCandidate], no_input: bool) -> Result<LocalIdlCandidate> {
    if candidates.len() == 1 {
        return Ok(candidates[0].clone());
    }
    if no_input || !std::io::stdin().is_terminal() {
        let paths = candidates
            .iter()
            .map(|candidate| display_path(&candidate.idl_path))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "multiple local Anchor IDLs match this program; use --idl or select one interactively: {paths}"
        );
    }
    let choices = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} ({})",
                display_path(&candidate.idl_path),
                display_path(&candidate.member_dir)
            )
        })
        .collect::<Vec<_>>();
    let selected = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a local Anchor IDL")
        .items(&choices)
        .default(0)
        .interact_opt()?
        .and_then(|index| candidates.get(index))
        .cloned();
    selected.ok_or_else(|| anyhow::anyhow!("no local IDL selected"))
}

fn confirm_registration(candidate: &LocalIdlCandidate) -> Result<bool> {
    Ok(Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Register discovered IDL {}?",
            display_path(&candidate.idl_path)
        ))
        .default(true)
        .interact_opt()?
        .unwrap_or(false))
}

/// Formats a local path compactly for terminal output without changing its value.
fn display_path(path: &Path) -> String {
    let Some(home) = BaseDirs::new().map(|base_dirs| base_dirs.home_dir().to_path_buf()) else {
        return path.display().to_string();
    };
    match path.strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Emits bytes according to the caller-selected presentation format.
pub(crate) fn print_bytes(bytes: &[u8], format: BytesFormat, decoded: &Value) -> Result<()> {
    match format {
        BytesFormat::Base64 => println!(
            "{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
        BytesFormat::Base58 => println!("{}", bs58::encode(bytes).into_string()),
        BytesFormat::Hex => println!("{}", hex::encode(bytes)),
        BytesFormat::Raw => {
            use std::io::Write;
            std::io::stdout().write_all(bytes)?;
        }
        BytesFormat::Breakdown => println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"length":bytes.len(),"data":decoded,"base64":base64::engine::general_purpose::STANDARD.encode(bytes)})
            )?
        ),
    }
    Ok(())
}

/// Writes a short, styled human status message to standard error.
pub(crate) fn print_status(message: &str) {
    eprintln!("{} {}", style("✓").green(), style(message).bold());
}

#[cfg(test)]
mod tests {
    use super::anchor_program_aliases;

    #[test]
    fn preserves_snake_case_and_adds_kebab_case() {
        assert_eq!(
            anchor_program_aliases("ncn_snapshot"),
            ["ncn_snapshot", "ncn-snapshot"]
        );
    }
}
