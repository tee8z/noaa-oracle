//! Configuration loading utilities
//!
//! Supports loading configuration from multiple sources in priority order:
//! 1. CLI arguments (highest priority)
//! 2. Environment variables
//! 3. Config file (searched in standard locations)
//! 4. Built-in defaults (lowest priority)

use std::env;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use serde::de::DeserializeOwned;

use crate::APP_NAME;

/// Describes where a configuration was loaded from
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigSource {
    /// Explicit path provided via CLI or env var
    Explicit(PathBuf),
    /// Found in current working directory
    CurrentDir(PathBuf),
    /// Found in XDG config home (~/.config/noaa-oracle/)
    XdgConfig(PathBuf),
    /// Found in system config (/etc/noaa-oracle/)
    System(PathBuf),
    /// No config file found, using defaults
    Defaults,
}

impl ConfigSource {
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            ConfigSource::Explicit(p) => Some(p),
            ConfigSource::CurrentDir(p) => Some(p),
            ConfigSource::XdgConfig(p) => Some(p),
            ConfigSource::System(p) => Some(p),
            ConfigSource::Defaults => None,
        }
    }
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::Explicit(p) => write!(f, "{}", p.display()),
            ConfigSource::CurrentDir(p) => write!(f, "{}", p.display()),
            ConfigSource::XdgConfig(p) => write!(f, "{}", p.display()),
            ConfigSource::System(p) => write!(f, "{}", p.display()),
            ConfigSource::Defaults => write!(f, "(defaults)"),
        }
    }
}

/// Find a configuration file in standard locations
///
/// Search order:
/// 1. Environment variable (e.g., NOAA_ORACLE_CONFIG or NOAA_DAEMON_CONFIG)
/// 2. Current directory (oracle.toml or daemon.toml)
/// 3. XDG config home ($XDG_CONFIG_HOME/noaa-oracle/ or ~/.config/noaa-oracle/)
/// 4. System config (/etc/noaa-oracle/)
///
/// # Arguments
/// * `env_var` - Environment variable to check for explicit path
/// * `filename` - Config filename to search for (e.g., "oracle.toml" or "daemon.toml")
pub fn find_config_file(env_var: &str, filename: &str) -> ConfigSource {
    // 1. Environment variable
    if let Ok(path) = env::var(env_var) {
        let p = PathBuf::from(&path);
        if p.exists() {
            return ConfigSource::Explicit(p);
        }
    }

    // 2. Current directory
    let local = PathBuf::from(filename);
    if local.exists() {
        return ConfigSource::CurrentDir(local);
    }

    // 3. XDG config home
    let xdg_path = get_xdg_config_path(filename);
    if xdg_path.exists() {
        return ConfigSource::XdgConfig(xdg_path);
    }

    // 4. System config
    let system = PathBuf::from(format!("/etc/{}/{}", APP_NAME, filename));
    if system.exists() {
        return ConfigSource::System(system);
    }

    ConfigSource::Defaults
}

/// Get the XDG config path for a given filename
fn get_xdg_config_path(filename: &str) -> PathBuf {
    if let Ok(xdg_config) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg_config).join(APP_NAME).join(filename)
    } else if let Ok(home) = env::var("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join(APP_NAME)
            .join(filename)
    } else {
        // Fallback - won't exist but keeps the code simple
        PathBuf::from(format!(".config/{}/{}", APP_NAME, filename))
    }
}

/// Get the XDG data directory for the application
pub fn get_xdg_data_dir() -> PathBuf {
    if let Ok(xdg_data) = env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg_data).join(APP_NAME)
    } else if let Ok(home) = env::var("HOME") {
        PathBuf::from(home).join(".local/share").join(APP_NAME)
    } else {
        PathBuf::from(format!(".local/share/{}", APP_NAME))
    }
}

/// Get the XDG cache directory for the application
pub fn get_xdg_cache_dir() -> PathBuf {
    if let Ok(xdg_cache) = env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg_cache).join(APP_NAME)
    } else if let Ok(home) = env::var("HOME") {
        PathBuf::from(home).join(".cache").join(APP_NAME)
    } else {
        PathBuf::from(format!(".cache/{}", APP_NAME))
    }
}

/// Load and parse a TOML configuration file
///
/// # Arguments
/// * `source` - The configuration source to load from
///
/// # Returns
/// * `Ok(Some(config))` - Successfully loaded and parsed config
/// * `Ok(None)` - No config file (using defaults)
/// * `Err(e)` - Failed to read or parse the config file
pub fn load_config<T: DeserializeOwned + Default>(source: &ConfigSource) -> anyhow::Result<T> {
    match source.path() {
        Some(path) => {
            let mut file = File::open(path)?;
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            let config: T = toml::from_str(&content)?;
            Ok(config)
        }
        None => Ok(T::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_source_display() {
        let source = ConfigSource::CurrentDir(PathBuf::from("test.toml"));
        assert_eq!(format!("{}", source), "test.toml");

        let source = ConfigSource::Defaults;
        assert_eq!(format!("{}", source), "(defaults)");
    }
}
