use anyhow::{anyhow, Error};
use async_compression::tokio::bufread::GzipDecoder;
use clap::Parser;
use futures::TryStreamExt;
use noaa_oracle_core::{
    find_config_file, load_config, ConfigSource, DEFAULT_FETCH_INTERVAL, DEFAULT_ORACLE_PORT,
};
use reqwest::Client;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use slog::{debug, error, info, o, Drain, Level, Logger};
use std::{
    env, fs,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;
use tokio_util::compat::FuturesAsyncReadCompatExt;

#[derive(Parser, Clone, Debug, serde::Deserialize, Default)]
#[command(
    author,
    version,
    about = "NOAA Daemon - Fetches weather data and uploads to oracle"
)]
pub struct Cli {
    /// Path to config file (TOML format)
    /// Searched in order: this flag, $NOAA_DAEMON_CONFIG, ./daemon.toml,
    /// $XDG_CONFIG_HOME/noaa-oracle/daemon.toml, /etc/noaa-oracle/daemon.toml
    #[arg(short, long)]
    #[serde(skip)]
    pub config: Option<String>,

    /// Log level: trace, debug, info, warn, error
    #[arg(short, long, env = "NOAA_DAEMON_LEVEL")]
    pub level: Option<String>,

    /// Oracle server URL to upload parquet files to
    #[arg(short, long, env = "NOAA_DAEMON_BASE_URL")]
    pub base_url: Option<String>,

    /// Local directory for temporary parquet storage before upload
    #[arg(short, long, env = "NOAA_DAEMON_DATA_DIR")]
    pub data_dir: Option<String>,

    /// Fetch interval in seconds (NOAA updates hourly)
    #[arg(short, long, env = "NOAA_DAEMON_SLEEP_INTERVAL")]
    pub sleep_interval: Option<u64>,

    /// Rate limiter refill rate in seconds
    #[arg(short, long, env = "NOAA_DAEMON_REFILL_RATE")]
    pub refill_rate: Option<f64>,

    /// Rate limiter token capacity
    #[arg(short, long, env = "NOAA_DAEMON_TOKEN_CAPACITY")]
    pub token_capacity: Option<usize>,

    /// HTTP User-Agent header for NOAA API requests
    #[arg(short, long, env = "NOAA_DAEMON_USER_AGENT")]
    pub user_agent: Option<String>,
}

impl Cli {
    /// Get the effective configuration value with defaults
    pub fn base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| format!("http://localhost:{}", DEFAULT_ORACLE_PORT))
    }

    pub fn data_dir(&self) -> String {
        self.data_dir
            .clone()
            .unwrap_or_else(|| "./data".to_string())
    }

    pub fn sleep_interval(&self) -> u64 {
        self.sleep_interval.unwrap_or(DEFAULT_FETCH_INTERVAL)
    }

    pub fn refill_rate(&self) -> f64 {
        self.refill_rate.unwrap_or(15.0)
    }

    pub fn token_capacity(&self) -> usize {
        self.token_capacity.unwrap_or(3)
    }

    pub fn user_agent(&self) -> String {
        self.user_agent
            .clone()
            .unwrap_or_else(|| "noaa-oracle-daemon/1.0".to_string())
    }
}

/// Load configuration from CLI args, config file, and environment
pub fn get_config_info() -> Cli {
    let cli_args = Cli::parse();

    // Determine config file path
    let source = if let Some(ref path) = cli_args.config {
        ConfigSource::Explicit(path.into())
    } else {
        find_config_file("NOAA_DAEMON_CONFIG", "daemon.toml")
    };

    // Load from config file
    let file_config: Cli = load_config(&source).unwrap_or_default();

    // CLI args override file config (env vars are handled by clap)
    Cli {
        config: cli_args.config,
        level: cli_args.level.or(file_config.level),
        base_url: cli_args.base_url.or(file_config.base_url),
        data_dir: cli_args.data_dir.or(file_config.data_dir),
        sleep_interval: cli_args.sleep_interval.or(file_config.sleep_interval),
        refill_rate: cli_args.refill_rate.or(file_config.refill_rate),
        token_capacity: cli_args.token_capacity.or(file_config.token_capacity),
        user_agent: cli_args.user_agent.or(file_config.user_agent),
    }
}

pub fn setup_logger(cli: &Cli) -> Logger {
    let log_level = if let Some(level) = cli.level.as_ref() {
        match level.to_lowercase().as_str() {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" => Level::Warning,
            "error" => Level::Error,
            _ => Level::Info,
        }
    } else {
        let rust_log = env::var("RUST_LOG").unwrap_or_default();
        match rust_log.to_lowercase().as_str() {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" => Level::Warning,
            "error" => Level::Error,
            _ => Level::Info,
        }
    };

    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::CompactFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let drain = drain.filter_level(log_level).fuse();
    slog::Logger::root(drain, o!("version" => env!("CARGO_PKG_VERSION")))
}

pub struct RateLimiter {
    capacity: usize,
    tokens: f64,
    last_refill: Instant,
    refill_rate: f64,
}

impl RateLimiter {
    pub fn new(capacity: usize, refill_rate: f64) -> Self {
        RateLimiter {
            capacity,
            tokens: capacity as f64,
            last_refill: Instant::now(),
            refill_rate,
        }
    }

    fn refill_tokens(&mut self) {
        let now = Instant::now();
        let elapsed_time = now.duration_since(self.last_refill).as_secs_f64();
        let tokens_to_add = elapsed_time * self.refill_rate;

        self.tokens += tokens_to_add.min(self.capacity as f64);
        self.last_refill = now;
    }

    fn try_acquire(&mut self, tokens: f64) -> bool {
        let mut retries = 0;

        loop {
            self.refill_tokens();

            if tokens <= self.tokens {
                self.tokens -= tokens;
                return true;
            } else {
                if retries >= 3 {
                    return false;
                }
                retries += 1;
                thread::sleep(Duration::from_secs(20));
            }
        }
    }
}

pub struct XmlFetcher {
    logger: Logger,
    user_agent: String,
    rate_limiter: Arc<Mutex<RateLimiter>>,
}

impl XmlFetcher {
    pub fn new(
        logger: Logger,
        user_agent: String,
        rate_limiter: Arc<Mutex<RateLimiter>>,
    ) -> XmlFetcher {
        Self {
            logger,
            user_agent,
            rate_limiter,
        }
    }

    pub async fn fetch_xml(&self, url: &str) -> Result<String, Error> {
        let mut limiter = self.rate_limiter.lock().await;
        if !limiter.try_acquire(1.0) {
            return Err(anyhow!("Rate limit exceeded after retries"));
        }

        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let client = ClientBuilder::new(Client::builder().user_agent(&self.user_agent).build()?)
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();

        debug!(self.logger, "requesting: {}", url);
        let response = client
            .get(url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| anyhow!("error sending request: {}", e))?;
        match response.text().await {
            Ok(xml_content) => Ok(xml_content),
            Err(e) => Err(anyhow!("error parsing body of request: {}", e)),
        }
    }

    pub async fn fetch_xml_gzip(&self, url: &str) -> Result<String, Error> {
        let mut limiter = self.rate_limiter.lock().await;
        if !limiter.try_acquire(1.0) {
            return Err(anyhow!("Rate limit exceeded after retries"));
        }

        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let client = ClientBuilder::new(Client::builder().user_agent(&self.user_agent).build()?)
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();

        debug!(self.logger, "requesting: {}", url);
        let response = client
            .get(url)
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .map_err(|e| anyhow!("error sending request: {}", e))?;
        if !response.status().is_success() {
            return Err(anyhow!("error response from request"));
        }

        let stream = response
            .bytes_stream()
            .map_err(|e| futures::io::Error::new(futures::io::ErrorKind::Other, e))
            .into_async_read()
            .compat();
        let gzip_decoder = GzipDecoder::new(stream);

        let buf_reader = tokio::io::BufReader::new(gzip_decoder);
        let mut content = String::new();
        let mut lines = buf_reader.lines();
        while let Some(line) = lines.next_line().await? {
            content.push_str(line.as_str());
            content.push('\n');
        }

        Ok(content)
    }
}

pub fn get_full_path(relative_path: String) -> String {
    let mut current_dir = env::current_dir().expect("Failed to get current directory");
    current_dir.push(relative_path);
    current_dir.to_string_lossy().to_string()
}

pub fn create_folder(root_path: &str, logger: &Logger) {
    let path = Path::new(root_path);

    if !path.exists() || !path.is_dir() {
        if let Err(err) = fs::create_dir_all(path) {
            error!(logger, "error creating folder: {}", err);
        } else {
            info!(logger, "folder created: {}", root_path);
        }
    } else {
        info!(logger, "folder already exists: {}", root_path);
    }
}

pub fn subfolder_exists(subfolder_path: &str) -> bool {
    fs::metadata(subfolder_path).is_ok()
}
