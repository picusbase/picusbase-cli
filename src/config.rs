use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_API_URL: &str = "http://localhost:4080/api";
const SUPPORTED_FUNCTION_CAPABILITIES: &[&str] =
    &["db.query", "db.query.read", "db.query.write", "http.fetch"];

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct CliProfile {
    pub url: Option<String>,
    pub executor_url: Option<String>,
    pub project_ref: Option<String>,
    pub personal_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct CliDefaults {
    pub url: Option<String>,
    pub executor_url: Option<String>,
    pub project_ref: Option<String>,
    pub personal_token: Option<String>,
    pub default_profile: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, CliProfile>,
}

impl CliDefaults {
    pub fn default_api_url() -> &'static str {
        DEFAULT_API_URL
    }

    pub fn path() -> Result<PathBuf> {
        if let Ok(path) = env::var("PICUSBASE_CONFIG_PATH") {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                anyhow::bail!("PICUSBASE_CONFIG_PATH must not be empty");
            }

            return Ok(PathBuf::from(trimmed));
        }

        if let Ok(path) = env::var("XDG_CONFIG_HOME") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed).join("picusbase").join("config.toml"));
            }
        }

        let home = env::var("HOME")
            .context("HOME is not set; set PICUSBASE_CONFIG_PATH to choose a config file")?;

        Ok(PathBuf::from(home)
            .join(".config")
            .join("picusbase")
            .join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read CLI config at {}", path.display()))?;

        toml::from_str(&content)
            .with_context(|| format!("Failed to parse CLI config at {}", path.display()))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path()?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create CLI config directory {}", parent.display())
            })?;
        }

        let content = toml::to_string_pretty(self).context("Failed to serialize CLI config")?;
        let content = if content.is_empty() {
            String::new()
        } else {
            format!("{}\n", content)
        };

        fs::write(&path, content)
            .with_context(|| format!("Failed to write CLI config at {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("Failed to restrict permissions on {}", path.display()))?;
        }

        Ok(path)
    }

    pub fn upsert_profile(&mut self, name: &str) -> &mut CliProfile {
        self.profiles.entry(name.to_string()).or_default()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PicusConfig {
    pub function: FunctionConfig,
    #[serde(default)]
    pub capabilities: CapabilityManifest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionConfig {
    pub id: String,
    pub name: String,
    pub entry_point: Option<String>,
    pub project_ref: Option<String>,
    pub domain: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct CapabilityManifest {
    pub allowed: Vec<String>,
}

impl CapabilityManifest {
    pub fn normalized_allowed(&self) -> Result<Vec<String>> {
        let mut normalized = BTreeSet::new();

        for capability in &self.allowed {
            let trimmed = capability.trim();
            if trimmed.is_empty() {
                anyhow::bail!("[capabilities].allowed must not contain empty entries");
            }

            if !SUPPORTED_FUNCTION_CAPABILITIES
                .iter()
                .any(|supported| supported == &trimmed)
            {
                anyhow::bail!(
                    "Unsupported capability '{}'. Supported values: {}",
                    trimmed,
                    SUPPORTED_FUNCTION_CAPABILITIES.join(", ")
                );
            }

            normalized.insert(trimmed.to_string());
        }

        Ok(normalized.into_iter().collect())
    }
}

impl PicusConfig {
    pub fn load_from_dir(path: &Path) -> Result<Self> {
        let config_path = path.join("picus.toml");
        if !config_path.exists() {
            anyhow::bail!("picus.toml configuration file not found in {:?}", path);
        }

        let content = fs::read_to_string(&config_path).context("Failed to read picus.toml")?;

        let config: PicusConfig = toml::from_str(&content).context("Failed to parse picus.toml")?;

        Ok(config)
    }

    pub fn allowed_capabilities(&self) -> Result<Vec<String>> {
        self.capabilities.normalized_allowed()
    }
}
