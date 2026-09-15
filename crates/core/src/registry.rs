//! Atomic local IDL registry storage and source resolution.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde_json::Value;
use tracing::{debug, warn};
use wip_types::{IdlSource, RegistryConfig};

use crate::idl::parse_idl;

/// Local registry plus its filesystem location.
#[derive(Clone, Debug)]
pub struct Registry {
    /// Where the TOML registry is read and written.
    pub path: PathBuf,
    /// In-memory source configuration.
    pub config: RegistryConfig,
}

/// A schema together with the IDL source that successfully produced it.
#[derive(Clone, Debug)]
pub struct LoadedSchema {
    /// Parsed program schema.
    pub schema: wip_types::ProgramSchema,
    /// Source selected after explicit-source or registry fallback resolution.
    pub source: IdlSource,
}

/// Returns the platform-appropriate persistent registry location.
pub fn default_registry_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("org", "solana", "wip")
        .ok_or_else(|| anyhow::anyhow!("cannot determine a platform config directory"))?;
    Ok(dirs.config_dir().join("registry.toml"))
}

impl Registry {
    /// Loads an existing registry or creates an empty in-memory registry.
    pub async fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or(default_registry_path()?);
        let config = match tokio::fs::read_to_string(&path).await {
            Ok(text) => {
                debug!(path = %path.display(), "loading IDL registry");
                toml_edit::de::from_str(&text).context("registry TOML is invalid")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "IDL registry does not exist; using empty registry");
                RegistryConfig {
                    schema_version: 1,
                    ..Default::default()
                }
            }
            Err(error) => return Err(error).context("cannot read registry"),
        };
        Ok(Self { path, config })
    }

    /// Persists the registry via a same-directory temporary file and rename.
    pub async fn save(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("registry path has no parent"))?;
        tokio::fs::create_dir_all(parent).await?;
        let text = toml_edit::ser::to_string_pretty(&self.config)?;
        let temporary = self.path.with_extension("toml.tmp");
        tokio::fs::write(&temporary, text).await?;
        tokio::fs::rename(temporary, &self.path).await?;
        debug!(path = %self.path.display(), "saved IDL registry");
        Ok(())
    }

    /// Resolves an alias or validates a base58 program address.
    pub fn program_address(&self, program: &str) -> Result<String> {
        if self.config.programs.contains_key(program) {
            debug!(program, "resolved program as registered address");
            return Ok(program.to_owned());
        }
        for (address, entry) in &self.config.programs {
            if entry.aliases.contains(program) {
                debug!(program, address, "resolved program alias");
                return Ok(address.clone());
            }
        }
        debug!(program, "using explicit program address");
        validate_address(program)?;
        Ok(program.to_owned())
    }

    /// Adds a source at highest precedence, retaining all lower-priority sources.
    pub fn add_source(
        &mut self,
        program: &str,
        source: IdlSource,
        alias: Option<String>,
    ) -> Result<()> {
        self.add_source_with_aliases(program, source, alias)
    }

    /// Adds a source at highest precedence and records all provided program aliases.
    pub fn add_source_with_aliases(
        &mut self,
        program: &str,
        source: IdlSource,
        aliases: impl IntoIterator<Item = String>,
    ) -> Result<()> {
        validate_address(program)?;
        let entry = self.config.programs.entry(program.to_owned()).or_default();
        for alias in aliases {
            entry.aliases.insert(alias);
        }
        entry.sources.retain(|existing| existing != &source);
        entry.sources.insert(0, source);
        debug!(program, aliases = ?entry.aliases, source_count = entry.sources.len(), "updated IDL registry entry");
        Ok(())
    }

    /// Reads the first usable local or HTTP source, attempting fallbacks in order.
    pub async fn load_schema(
        &self,
        program: &str,
        explicit_idl: Option<&Path>,
    ) -> Result<wip_types::ProgramSchema> {
        Ok(self
            .load_schema_with_source(program, explicit_idl)
            .await?
            .schema)
    }

    /// Reads a schema and returns the source that successfully produced it.
    pub async fn load_schema_with_source(
        &self,
        program: &str,
        explicit_idl: Option<&Path>,
    ) -> Result<LoadedSchema> {
        let address = self.program_address(program)?;
        debug!(program, address, explicit_idl = ?explicit_idl, "resolving program schema");
        if let Some(path) = explicit_idl {
            let text = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("cannot read IDL {}", path.display()))?;
            let schema = parse_idl(&text, &address)?;
            debug!(path = %path.display(), schema = %schema.name, "loaded explicit IDL");
            return Ok(LoadedSchema {
                schema,
                source: IdlSource::File {
                    name: path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                    path: path.to_path_buf(),
                },
            });
        }
        let entry = self.config.programs.get(&address).ok_or_else(|| {
            anyhow::anyhow!(
                "no IDL registered for {address}; run `wip idl add {address} --file FILE`"
            )
        })?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()?;
        let mut errors = Vec::new();
        for source in &entry.sources {
            debug!(address, source = ?source, "trying registered IDL source");
            let text = match source {
                IdlSource::File { path, .. } => tokio::fs::read_to_string(path)
                    .await
                    .map_err(anyhow::Error::from),
                IdlSource::Url { url, .. } => {
                    if !matches!(url.scheme(), "http" | "https") {
                        bail!("IDL URLs must use HTTP(S)")
                    }
                    client
                        .get(url.clone())
                        .send()
                        .await?
                        .error_for_status()?
                        .text()
                        .await
                        .map_err(anyhow::Error::from)
                }
                IdlSource::OnChain { .. } => Err(anyhow::anyhow!(
                    "on-chain IDL fetch is not available in this build; register a file or URL source"
                )),
            };
            match text.and_then(|text| parse_idl(&text, &address)) {
                Ok(schema) => {
                    debug!(address, schema = %schema.name, "loaded registered IDL source");
                    return Ok(LoadedSchema {
                        schema,
                        source: source.clone(),
                    });
                }
                Err(error) => {
                    warn!(address, source = ?source, error = %error, "registered IDL source failed");
                    errors.push(error.to_string());
                }
            }
        }
        bail!("no registered IDL source succeeded: {}", errors.join("; "))
    }

    /// Displays registry configuration as deterministic JSON for humans and scripts.
    pub fn json(&self) -> Value {
        serde_json::to_value(&self.config).expect("registry config serializes")
    }
}

/// Validates that a value is a 32-byte base58 Solana public key.
pub fn validate_address(value: &str) -> Result<()> {
    let bytes = bs58::decode(value)
        .into_vec()
        .context("address is not base58")?;
    if bytes.len() != 32 {
        bail!("address must decode to 32 bytes")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: &str = "11111111111111111111111111111111";

    #[test]
    fn records_all_discovered_aliases() {
        let mut registry = Registry {
            path: PathBuf::from("registry.toml"),
            config: RegistryConfig::default(),
        };
        registry
            .add_source_with_aliases(
                ADDRESS,
                IdlSource::File {
                    name: "demo".to_owned(),
                    path: PathBuf::from("demo.json"),
                },
                ["ncn_snapshot".to_owned(), "ncn-snapshot".to_owned()],
            )
            .unwrap();
        assert_eq!(registry.program_address("ncn_snapshot").unwrap(), ADDRESS);
        assert_eq!(registry.program_address("ncn-snapshot").unwrap(), ADDRESS);
    }

    #[tokio::test]
    async fn reports_the_explicit_idl_source_that_loaded_the_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("example.json");
        tokio::fs::write(
            &path,
            r#"{"name":"example","accounts":[],"instructions":[{"name":"ping","discriminator":[1,2,3,4,5,6,7,8],"accounts":[],"args":[]}]}"#,
        )
        .await
        .unwrap();
        let registry = Registry {
            path: directory.path().join("registry.toml"),
            config: RegistryConfig::default(),
        };

        let loaded = registry
            .load_schema_with_source(ADDRESS, Some(&path))
            .await
            .unwrap();

        assert_eq!(loaded.schema.name, "example");
        assert!(matches!(loaded.source, IdlSource::File { path: source, .. } if source == path));
    }
}
