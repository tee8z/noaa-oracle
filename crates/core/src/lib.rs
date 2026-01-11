//! NOAA Oracle Core Library
//!
//! Shared utilities for the oracle and daemon services:
//! - Configuration loading (XDG-compliant)
//! - File system utilities
//! - Common types

mod config;
pub mod fs;

pub use config::{
    find_config_file, get_xdg_cache_dir, get_xdg_data_dir, load_config, ConfigSource,
};
pub use fs::{create_dir_all, ensure_dir_exists, is_directory, path_exists};

/// Application name used for XDG paths
pub const APP_NAME: &str = "noaa-oracle";

/// Default oracle port
pub const DEFAULT_ORACLE_PORT: u16 = 9800;

/// Default daemon fetch interval (1 hour)
pub const DEFAULT_FETCH_INTERVAL: u64 = 3600;
