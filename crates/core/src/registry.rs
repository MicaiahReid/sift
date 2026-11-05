//! Atomic local IDL registry storage and source resolution.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde_json::Value;
use sift_types::{IdlSource, RegistryConfig};
use tracing::{debug, warn};

use crate::{RpcClient, idl::parse_idl, load_canonical_idl};

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
    pub schema: sift_types::ProgramSchema,
    /// Source selected after explicit-source or registry fallback resolution.
    pub source: IdlSource,
}

/// Returns the platform-appropriate persistent registry location.
pub fn default_registry_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("org", "solana", "sift")
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
        let source_id = entry
            .sources
            .iter()
            .find_map(|(id, existing)| (existing == &source).then(|| id.clone()))
            .unwrap_or_else(|| next_source_id(&entry.sources, &source));
        entry.sources.insert(source_id.clone(), source);
        entry.priority.retain(|id| id != &source_id);
        entry.priority.insert(0, source_id);
        debug!(program, aliases = ?entry.aliases, source_count = entry.sources.len(), "updated IDL registry entry");
        Ok(())
    }

    /// Moves a configured source to the highest priority.
    pub fn prefer_source(&mut self, program: &str, source_id: &str) -> Result<()> {
        let address = self.program_address(program)?;
        let entry = self
            .config
            .programs
            .get_mut(&address)
            .ok_or_else(|| anyhow::anyhow!("program {program} is not registered"))?;
        if !entry.sources.contains_key(source_id) {
            bail!("program {program} has no source named {source_id}")
        }
        entry.priority.retain(|id| id != source_id);
        entry.priority.insert(0, source_id.to_owned());
        Ok(())
    }

    /// Reads the first usable local or HTTP source, attempting fallbacks in order.
    pub async fn load_schema(
        &self,
        program: &str,
        explicit_idl: Option<&Path>,
        rpc: &RpcClient,
    ) -> Result<sift_types::ProgramSchema> {
        Ok(self
            .load_schema_with_source(program, explicit_idl, rpc)
            .await?
            .schema)
    }

    /// Reads a schema and returns the source that successfully produced it.
    pub async fn load_schema_with_source(
        &self,
        program: &str,
        explicit_idl: Option<&Path>,
        rpc: &RpcClient,
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
                "no IDL registered for {address}; run `sift idl add {address} --file FILE`"
            )
        })?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()?;
        let mut errors = Vec::new();
        for source_id in &entry.priority {
            let Some(source) = entry.sources.get(source_id) else {
                errors.push(format!("priority references unknown source {source_id}"));
                continue;
            };
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
                IdlSource::OnChain {
                    address: metadata_address,
                    ..
                } => load_canonical_idl(rpc, &address, metadata_address)
                    .await
                    .map(|idl| idl.text),
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

fn next_source_id(
    sources: &std::collections::BTreeMap<String, IdlSource>,
    source: &IdlSource,
) -> String {
    let prefix = match source {
        IdlSource::File { .. } => "local",
        IdlSource::Url { .. } => "url",
        IdlSource::OnChain { .. } => "onchain",
    };
    for number in 1.. {
        let id = format!("{prefix}-{number}");
        if !sources.contains_key(&id) {
            return id;
        }
    }
    unreachable!()
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

    #[test]
    fn promotes_a_stable_source_id_without_losing_fallbacks() {
        let mut registry = Registry {
            path: PathBuf::from("registry.toml"),
            config: RegistryConfig::default(),
        };
        registry
            .add_source(
                ADDRESS,
                IdlSource::File {
                    name: "local".to_owned(),
                    path: PathBuf::from("local.json"),
                },
                None,
            )
            .unwrap();
        registry
            .add_source(
                ADDRESS,
                IdlSource::Url {
                    name: "remote".to_owned(),
                    url: "https://example.test/idl.json".parse().unwrap(),
                },
                None,
            )
            .unwrap();
        registry.prefer_source(ADDRESS, "local-1").unwrap();
        assert_eq!(
            registry.config.programs.get(ADDRESS).unwrap().priority,
            ["local-1", "url-1"]
        );
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

        let rpc = RpcClient::new(
            Some("http://127.0.0.1:1".to_owned()),
            "confirmed".to_owned(),
        )
        .unwrap();
        let loaded = registry
            .load_schema_with_source(ADDRESS, Some(&path), &rpc)
            .await
            .unwrap();

        assert_eq!(loaded.schema.name, "example");
        assert!(matches!(loaded.source, IdlSource::File { path: source, .. } if source == path));
    }
}
