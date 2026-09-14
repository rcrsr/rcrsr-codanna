//! Team profile configuration stored at .codanna/profiles.json
//!
//! Three-tier configuration system:
//! 1. Global: ~/.codanna/providers.json (user's registered providers)
//! 2. Team: .codanna/profiles.json (team contract, committed to git)
//! 3. Local: .codanna/profiles.lock.json (installation state, not committed)

use super::error::{ProfileError, ProfileResult};
use super::provider_registry::ProviderSource;
use super::reference::ProfileReference;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Team profile configuration
/// Follows Claude Code's plugin configuration pattern
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfilesConfig {
    /// Configuration version
    #[serde(default = "default_version")]
    pub version: u32,

    /// Additional providers to register (extends global providers)
    /// Maps provider_id -> provider source
    #[serde(
        default,
        rename = "extraKnownProviders",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub extra_known_providers: HashMap<String, ExtraProvider>,

    /// Required profiles (format: "name@provider")
    #[serde(default)]
    pub profiles: Vec<String>,
}

/// Extra provider definition for team configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExtraProvider {
    pub source: ProviderSource,
}

fn default_version() -> u32 {
    1
}

impl ProfilesConfig {
    /// Create a new empty configuration
    pub fn new() -> Self {
        Self {
            version: 1,
            extra_known_providers: HashMap::new(),
            profiles: Vec::new(),
        }
    }

    /// Load configuration from disk
    pub fn load(path: &Path) -> ProfileResult<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }

        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Save configuration to disk
    pub fn save(&self, path: &Path) -> ProfileResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Add a profile to the configuration
    pub fn add_profile(&mut self, profile_ref: &str) {
        if !self.profiles.contains(&profile_ref.to_string()) {
            self.profiles.push(profile_ref.to_string());
        }
    }

    /// Remove a profile from the configuration
    pub fn remove_profile(&mut self, profile_ref: &str) -> bool {
        if let Some(pos) = self.profiles.iter().position(|p| p == profile_ref) {
            self.profiles.remove(pos);
            true
        } else {
            false
        }
    }

    /// Parse profile references
    pub fn parse_profiles(&self) -> Vec<ProfileReference> {
        self.profiles
            .iter()
            .map(|s| ProfileReference::parse(s))
            .collect()
    }

    /// Check if configuration has any profiles
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// Get list of provider IDs needed by this configuration
    pub fn get_required_provider_ids(&self) -> Vec<String> {
        let mut providers = Vec::new();

        // From extraKnownProviders
        for provider_id in self.extra_known_providers.keys() {
            providers.push(provider_id.clone());
        }

        // From profile references (name@provider)
        for profile_ref in &self.profiles {
            let reference = ProfileReference::parse(profile_ref);
            if let Some(provider) = reference.provider
                && !providers.contains(&provider)
            {
                providers.push(provider);
            }
        }

        providers
    }
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Legacy project manifest (backwards compatibility)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectManifest {
    pub profile: String,
}

impl ProjectManifest {
    /// Create a new empty manifest
    pub fn new() -> Self {
        Self {
            profile: String::new(),
        }
    }

    /// Load manifest from disk
    pub fn load(path: &Path) -> ProfileResult<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_json(&content)
    }

    /// Save manifest to disk
    pub fn save(&self, path: &Path) -> ProfileResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load manifest or create new if doesn't exist
    pub fn load_or_create(path: &Path) -> ProfileResult<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::new())
        }
    }

    /// Parse from JSON string
    pub fn from_json(json: &str) -> ProfileResult<Self> {
        let manifest: Self = serde_json::from_str(json)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> ProfileResult<()> {
        if self.profile.is_empty() {
            return Err(ProfileError::InvalidManifest {
                reason: "Profile name cannot be empty".to_string(),
            });
        }
        Ok(())
    }
}

impl Default for ProjectManifest {
    fn default() -> Self {
        Self::new()
    }
}
