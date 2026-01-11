//! Filesystem utilities

use std::fs;
use std::path::Path;

use log::{error, info};

/// Create a directory and all parent directories if they don't exist
///
/// This is a wrapper around `std::fs::create_dir_all` with logging.
pub fn create_dir_all(path: &str) -> std::io::Result<()> {
    let path = Path::new(path);
    if !path.exists() {
        fs::create_dir_all(path)?;
        info!("Created directory: {}", path.display());
    }
    Ok(())
}

/// Ensure a directory exists, creating it if necessary
///
/// Returns true if the directory exists (either already existed or was created).
pub fn ensure_dir_exists(path: &str) -> bool {
    let path = Path::new(path);

    if path.exists() && path.is_dir() {
        return true;
    }

    match fs::create_dir_all(path) {
        Ok(_) => {
            info!("Created directory: {}", path.display());
            true
        }
        Err(e) => {
            error!("Failed to create directory {}: {}", path.display(), e);
            false
        }
    }
}

/// Check if a path exists
pub fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Check if a path is a directory
pub fn is_directory(path: &str) -> bool {
    Path::new(path).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_exists() {
        // Current directory should exist
        assert!(path_exists("."));

        // Random path should not exist
        assert!(!path_exists("/nonexistent/path/12345"));
    }

    #[test]
    fn test_is_directory() {
        assert!(is_directory("."));
    }
}
