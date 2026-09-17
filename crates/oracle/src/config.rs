//! Configuration from CLI flags, environment variables, and a TOML file,
//! plus process logging setup.

use clap::Parser;
use fern::{
    Dispatch,
    colors::{Color, ColoredLevelConfig},
};
use log::LevelFilter;
use noaa_oracle_core::{ConfigSource, DEFAULT_ORACLE_PORT, find_config_file, load_config};
use std::{
    env,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Iso8601};

#[derive(Parser, Clone, Debug, serde::Deserialize, Default)]
#[command(
    author,
    version,
    about = "NOAA Oracle - Weather data oracle and DLC attestation service"
)]
pub struct Cli {
    /// Path to config file (TOML format)
    /// Searched in order: this flag, $NOAA_ORACLE_CONFIG, ./oracle.toml,
    /// $XDG_CONFIG_HOME/noaa-oracle/oracle.toml, /etc/noaa-oracle/oracle.toml
    #[arg(short, long)]
    #[serde(skip)]
    pub config: Option<String>,

    /// Log level: trace, debug, info, warn, error
    #[arg(short, long, env = "NOAA_ORACLE_LEVEL")]
    pub level: Option<String>,

    /// Host to listen on (use 0.0.0.0 for all interfaces)
    #[arg(short, long, env = "NOAA_ORACLE_HOST")]
    #[serde(alias = "host")]
    pub domain: Option<String>,

    /// Port to listen on
    #[arg(short, long, env = "NOAA_ORACLE_PORT")]
    pub port: Option<String>,

    /// Public URL for API responses and UI
    #[arg(short, long, env = "NOAA_ORACLE_REMOTE_URL")]
    pub remote_url: Option<String>,

    /// Directory containing weather parquet files
    /// Can point to pre-existing data from another source
    #[arg(short, long, env = "NOAA_ORACLE_DATA_DIR")]
    #[serde(alias = "data_dir")]
    pub weather_dir: Option<String>,

    /// Directory for DLC event database
    #[arg(short, long, env = "NOAA_ORACLE_EVENT_DB")]
    pub event_db: Option<String>,

    /// Directory containing UI static files
    #[arg(short, long, env = "NOAA_ORACLE_UI_DIR")]
    pub ui_dir: Option<String>,

    /// Path to oracle signing key (ECDSA secp256k1 PEM)
    #[arg(short, long, env = "NOAA_ORACLE_PRIVATE_KEY")]
    #[serde(alias = "private_key_path")]
    pub oracle_private_key: Option<String>,

    /// S3 bucket name for fetching weather data files
    /// When set, files are listed and served from S3 instead of local disk
    #[arg(long, env = "NOAA_ORACLE_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    /// Custom S3 endpoint URL (for MinIO or other S3-compatible storage)
    #[arg(long, env = "NOAA_ORACLE_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    /// Seconds to wait for HTTP requests, background work, and accepted
    /// writes to finish after a stop signal before giving up
    #[arg(long, env = "NOAA_ORACLE_SHUTDOWN_TIMEOUT")]
    pub shutdown_timeout: Option<u64>,
}

/// Validated settings the application starts from.
#[derive(Clone, Debug)]
pub struct Configuration {
    pub listen: SocketAddr,
    pub remote_url: String,
    pub weather_dir: PathBuf,
    pub event_dir: PathBuf,
    pub static_dir: PathBuf,
    pub private_key: PathBuf,
    pub s3_bucket: Option<String>,
    pub s3_endpoint: Option<String>,
    pub shutdown_timeout: Duration,
}

impl Cli {
    /// Get the effective configuration value with defaults
    pub fn host(&self) -> String {
        self.domain
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    pub fn port(&self) -> String {
        self.port
            .clone()
            .unwrap_or_else(|| DEFAULT_ORACLE_PORT.to_string())
    }

    pub fn remote_url(&self) -> String {
        self.remote_url
            .clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host(), self.port()))
    }

    pub fn weather_dir(&self) -> String {
        self.weather_dir
            .clone()
            .unwrap_or_else(|| "./weather_data".to_string())
    }

    pub fn event_db(&self) -> String {
        self.event_db
            .clone()
            .unwrap_or_else(|| "./event_data".to_string())
    }

    pub fn static_dir(&self) -> String {
        self.ui_dir
            .clone()
            // Fall back to compile-time path (where build.rs outputs files)
            .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/static").to_string())
    }

    pub fn private_key(&self) -> String {
        self.oracle_private_key
            .clone()
            .unwrap_or_else(|| "./oracle_private_key.pem".to_string())
    }

    pub fn shutdown_timeout(&self) -> u64 {
        self.shutdown_timeout.unwrap_or(25)
    }

    /// Validates addresses and bounds before anything is opened.
    pub fn configuration(&self) -> anyhow::Result<Configuration> {
        let host: IpAddr = self
            .host()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid host {:?}: {e}", self.host()))?;
        let port: u16 = self
            .port()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid port {:?}: {e}", self.port()))?;
        let shutdown_timeout = self.shutdown_timeout();
        anyhow::ensure!(shutdown_timeout > 0, "shutdown timeout must be positive");
        Ok(Configuration {
            listen: SocketAddr::new(host, port),
            remote_url: self.remote_url(),
            weather_dir: PathBuf::from(self.weather_dir()),
            event_dir: PathBuf::from(self.event_db()),
            static_dir: PathBuf::from(self.static_dir()),
            private_key: PathBuf::from(self.private_key()),
            s3_bucket: self.s3_bucket.clone().filter(|bucket| !bucket.is_empty()),
            s3_endpoint: self
                .s3_endpoint
                .clone()
                .filter(|endpoint| !endpoint.is_empty()),
            shutdown_timeout: Duration::from_secs(shutdown_timeout),
        })
    }
}

/// Load configuration from CLI args, config file, and environment
pub fn get_config_info() -> Cli {
    let cli_args = Cli::parse();

    // Determine config file path
    let source = if let Some(ref path) = cli_args.config {
        ConfigSource::Explicit(path.into())
    } else {
        find_config_file("NOAA_ORACLE_CONFIG", "oracle.toml")
    };

    // Log where we're loading config from
    if let Some(path) = source.path() {
        log::info!("Loading config from: {}", path.display());
    }

    // Load from config file
    let file_config: Cli = load_config(&source).unwrap_or_default();

    // CLI args override file config (env vars are handled by clap)
    Cli {
        config: cli_args.config,
        level: cli_args.level.or(file_config.level),
        domain: cli_args.domain.or(file_config.domain),
        port: cli_args.port.or(file_config.port),
        remote_url: cli_args.remote_url.or(file_config.remote_url),
        weather_dir: cli_args.weather_dir.or(file_config.weather_dir),
        event_db: cli_args.event_db.or(file_config.event_db),
        ui_dir: cli_args.ui_dir.or(file_config.ui_dir),
        oracle_private_key: cli_args
            .oracle_private_key
            .or(file_config.oracle_private_key),
        s3_bucket: cli_args.s3_bucket.or(file_config.s3_bucket),
        s3_endpoint: cli_args.s3_endpoint.or(file_config.s3_endpoint),
        shutdown_timeout: cli_args.shutdown_timeout.or(file_config.shutdown_timeout),
    }
}

pub fn get_log_level(cli: &Cli) -> LevelFilter {
    let level_str = cli
        .level
        .clone()
        .or_else(|| env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "info".to_string());

    match level_str.to_lowercase().as_str() {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "info" => LevelFilter::Info,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => LevelFilter::Info,
    }
}

pub fn setup_logger() -> Dispatch {
    let colors = ColoredLevelConfig::new()
        .trace(Color::White)
        .debug(Color::Cyan)
        .info(Color::Blue)
        .warn(Color::Yellow)
        .error(Color::Magenta);

    fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "[{} {}] {}: {}",
                OffsetDateTime::now_utc()
                    .format(&Iso8601::DEFAULT)
                    .unwrap_or_default(),
                colors.color(record.level()),
                record.target(),
                message
            ));
        })
        .chain(std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_produce_a_local_listener() {
        let configuration = Cli::default().configuration().unwrap();
        assert_eq!(configuration.listen.to_string(), "127.0.0.1:9800");
        assert_eq!(configuration.remote_url, "http://127.0.0.1:9800");
        assert_eq!(configuration.shutdown_timeout, Duration::from_secs(25));
        assert!(configuration.s3_bucket.is_none());
    }

    #[test]
    fn invalid_values_are_rejected_before_startup() {
        let bad_port = Cli {
            port: Some("http".into()),
            ..Cli::default()
        };
        assert!(bad_port.configuration().is_err());
        let bad_host = Cli {
            domain: Some("localhost".into()),
            ..Cli::default()
        };
        assert!(bad_host.configuration().is_err());
        let bad_timeout = Cli {
            shutdown_timeout: Some(0),
            ..Cli::default()
        };
        assert!(bad_timeout.configuration().is_err());
    }
}
