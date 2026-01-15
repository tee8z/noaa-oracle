use clap::Parser;
use fern::{
    colors::{Color, ColoredLevelConfig},
    Dispatch,
};
use log::LevelFilter;
use noaa_oracle_core::{
    find_config_file, load_config, path_exists, ConfigSource, DEFAULT_ORACLE_PORT,
};
use std::env;
use time::{format_description::well_known::Iso8601, OffsetDateTime};

pub use noaa_oracle_core::{create_dir_all, ensure_dir_exists};

/// Create a folder (legacy wrapper for compatibility)
pub fn create_folder(root_path: &str) {
    let _ = create_dir_all(root_path);
}

/// Check if a subfolder exists (legacy wrapper)
pub fn subfolder_exists(subfolder_path: &str) -> bool {
    path_exists(subfolder_path)
}

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
            .unwrap_or_else(|| "./static".to_string())
    }

    pub fn private_key(&self) -> String {
        self.oracle_private_key
            .clone()
            .unwrap_or_else(|| "./oracle_private_key.pem".to_string())
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
                OffsetDateTime::now_utc().format(&Iso8601::DEFAULT).unwrap(),
                colors.color(record.level()),
                record.target(),
                message
            ));
        })
        .chain(std::io::stdout())
}
