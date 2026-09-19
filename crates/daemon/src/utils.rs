//! Configuration, logging, and bounded HTTP fetching for NOAA sources.

use async_compression::tokio::bufread::GzipDecoder;
use clap::Parser;
use futures::{StreamExt, TryStreamExt};
use noaa_oracle_core::{
    ConfigSource, DEFAULT_FETCH_INTERVAL, DEFAULT_ORACLE_PORT, DEFAULT_USER_AGENT,
    find_config_file, load_config,
};
use reqwest::{Client, StatusCode, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use slog::{Drain, Level, Logger, debug, o};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, sync::Mutex};
use tokio_util::compat::FuturesAsyncReadCompatExt;

/// Largest response body accepted from NOAA, compressed or not.
pub const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest decompressed gzip body; a larger one is treated as hostile.
pub const MAX_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser, Clone, Debug, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
    pub config: Option<PathBuf>,

    /// Log level: trace, debug, info, warn, error
    #[arg(short, long, env = "NOAA_DAEMON_LEVEL")]
    pub level: Option<String>,

    /// Oracle public URL (scheme://host[:port]); must match the oracle's
    /// `remote_url`, because uploads are signed over it
    #[arg(short, long, env = "NOAA_DAEMON_BASE_URL")]
    pub base_url: Option<String>,

    /// Local directory for parquet files before and after upload
    #[arg(short, long, env = "NOAA_DAEMON_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Fetch interval in seconds (NOAA updates hourly)
    #[arg(short, long, env = "NOAA_DAEMON_SLEEP_INTERVAL")]
    pub sleep_interval: Option<u64>,

    /// Seconds for the rate limiter to refill `token_capacity` requests
    #[arg(short, long, env = "NOAA_DAEMON_REFILL_RATE")]
    pub refill_rate: Option<f64>,

    /// Requests allowed per `refill_rate` seconds
    #[arg(short, long, env = "NOAA_DAEMON_TOKEN_CAPACITY")]
    pub token_capacity: Option<usize>,

    /// HTTP User-Agent header for NOAA API requests
    #[arg(short, long, env = "NOAA_DAEMON_USER_AGENT")]
    pub user_agent: Option<String>,

    /// Path to the daemon's upload signing key (secp256k1 PEM, mode 0600;
    /// created if missing). Its npub goes in the oracle's uploader_pubkeys.
    #[arg(long, env = "NOAA_DAEMON_PRIVATE_KEY")]
    pub private_key: Option<PathBuf>,

    /// Fraction of stations that must have forecasts before a run publishes
    #[arg(long, env = "NOAA_DAEMON_MIN_FORECAST_COVERAGE")]
    pub min_forecast_coverage: Option<f64>,

    /// Days to keep published parquet files locally
    #[arg(long, env = "NOAA_DAEMON_RETENTION_DAYS")]
    pub retention_days: Option<u64>,

    /// S3 bucket for parquet archival
    #[arg(long, env = "NOAA_DAEMON_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    /// S3 endpoint URL (for moto/localstack, leave unset for AWS)
    #[arg(long, env = "NOAA_DAEMON_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,
}

/// Validated settings.
#[derive(Clone, Debug)]
pub struct Configuration {
    pub level: Level,
    pub base_url: Url,
    pub data_dir: PathBuf,
    pub interval: Duration,
    pub refill_period: Duration,
    pub token_capacity: usize,
    pub user_agent: String,
    pub private_key: PathBuf,
    pub min_forecast_coverage: f64,
    pub retention: Duration,
    pub s3: Option<S3Settings>,
}

#[derive(Clone, Debug)]
pub struct S3Settings {
    pub bucket: String,
    pub endpoint: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}")]
    File {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
    #[error("invalid base_url {0:?}: expected http(s)://host[:port] without a path")]
    BaseUrl(String),
    #[error("invalid log level {0:?}")]
    Level(String),
    #[error("{0} must be positive")]
    NotPositive(&'static str),
    #[error("min_forecast_coverage must be between 0 and 1")]
    Coverage,
}

impl Cli {
    /// Parses flags and merges them over the config file; flags and
    /// environment variables win. A config file that exists but does not
    /// parse is an error, never silently ignored.
    pub fn load() -> Result<Self, ConfigError> {
        let cli = Cli::parse();
        let source = match &cli.config {
            Some(path) => ConfigSource::Explicit(path.clone()),
            None => find_config_file("NOAA_DAEMON_CONFIG", "daemon.toml"),
        };
        let file: Cli = match source.path() {
            Some(path) => load_config(&source).map_err(|source| ConfigError::File {
                path: path.clone(),
                source,
            })?,
            None => Cli::default(),
        };
        Ok(Cli {
            config: cli.config,
            level: cli.level.or(file.level),
            base_url: cli.base_url.or(file.base_url),
            data_dir: cli.data_dir.or(file.data_dir),
            sleep_interval: cli.sleep_interval.or(file.sleep_interval),
            refill_rate: cli.refill_rate.or(file.refill_rate),
            token_capacity: cli.token_capacity.or(file.token_capacity),
            user_agent: cli.user_agent.or(file.user_agent),
            private_key: cli.private_key.or(file.private_key),
            min_forecast_coverage: cli.min_forecast_coverage.or(file.min_forecast_coverage),
            retention_days: cli.retention_days.or(file.retention_days),
            s3_bucket: cli.s3_bucket.or(file.s3_bucket),
            s3_endpoint: cli.s3_endpoint.or(file.s3_endpoint),
        })
    }

    pub fn configuration(&self) -> Result<Configuration, ConfigError> {
        let level = match self
            .level
            .as_deref()
            .unwrap_or("info")
            .to_lowercase()
            .as_str()
        {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" => Level::Warning,
            "error" => Level::Error,
            other => return Err(ConfigError::Level(other.to_owned())),
        };
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| format!("http://localhost:{DEFAULT_ORACLE_PORT}"));
        let base_url = validate_origin(&base_url)?;
        let positive_seconds = |value: f64, name| {
            if value.is_finite() && value > 0.0 {
                Ok(Duration::from_secs_f64(value))
            } else {
                Err(ConfigError::NotPositive(name))
            }
        };
        let token_capacity = self.token_capacity.unwrap_or(3);
        if token_capacity == 0 {
            return Err(ConfigError::NotPositive("token_capacity"));
        }
        let min_forecast_coverage = self.min_forecast_coverage.unwrap_or(0.8);
        if !(0.0..=1.0).contains(&min_forecast_coverage) {
            return Err(ConfigError::Coverage);
        }
        let retention_days = self.retention_days.unwrap_or(7);
        if retention_days == 0 {
            return Err(ConfigError::NotPositive("retention_days"));
        }
        Ok(Configuration {
            level,
            base_url,
            data_dir: self
                .data_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./data")),
            interval: positive_seconds(
                self.sleep_interval.unwrap_or(DEFAULT_FETCH_INTERVAL) as f64,
                "sleep_interval",
            )?,
            refill_period: positive_seconds(self.refill_rate.unwrap_or(15.0), "refill_rate")?,
            token_capacity,
            user_agent: self
                .user_agent
                .clone()
                .unwrap_or_else(|| DEFAULT_USER_AGENT.to_owned()),
            private_key: self
                .private_key
                .clone()
                .unwrap_or_else(|| PathBuf::from("./daemon_private_key.pem")),
            min_forecast_coverage,
            retention: Duration::from_secs(retention_days * 24 * 60 * 60),
            s3: self
                .s3_bucket
                .clone()
                .filter(|bucket| !bucket.is_empty())
                .map(|bucket| S3Settings {
                    bucket,
                    endpoint: self
                        .s3_endpoint
                        .clone()
                        .filter(|endpoint| !endpoint.is_empty()),
                }),
        })
    }
}

/// Accepts `http(s)://host[:port]` without credentials, path, or query.
fn validate_origin(url: &str) -> Result<Url, ConfigError> {
    let invalid = || ConfigError::BaseUrl(url.to_owned());
    let parsed = Url::parse(url.trim_end_matches('/')).map_err(|_| invalid())?;
    let origin_only = matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none();
    if origin_only {
        Ok(parsed)
    } else {
        Err(invalid())
    }
}

pub fn setup_logger(level: Level) -> Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::CompactFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let drain = drain.filter_level(level).fuse();
    slog::Logger::root(drain, o!("version" => env!("CARGO_PKG_VERSION")))
}

/// Token bucket: `capacity` requests per `period`, refilled continuously.
pub struct RateLimiter {
    capacity: f64,
    tokens: f64,
    per_second: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(capacity: usize, period: Duration) -> Self {
        let capacity = capacity as f64;
        RateLimiter {
            capacity,
            tokens: capacity,
            per_second: capacity / period.as_secs_f64(),
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.capacity);
        self.last_refill = now;
    }

    /// Takes a token, or returns how long until one is available.
    fn try_take(&mut self, now: Instant) -> Result<(), Duration> {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64(
                (1.0 - self.tokens) / self.per_second,
            ))
        }
    }

    /// Waits for a token. The lock is held only for the accounting, never
    /// across the wait or the request.
    pub async fn acquire(limiter: &Mutex<RateLimiter>) {
        loop {
            let wait = match limiter.lock().await.try_take(Instant::now()) {
                Ok(()) => return,
                Err(wait) => wait,
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("request to {url} failed")]
    Request {
        url: String,
        #[source]
        source: reqwest_middleware::Error,
    },
    #[error("reading the response from {url} failed")]
    Body {
        url: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{url} answered {status}")]
    Status { url: String, status: StatusCode },
    #[error("{url} returned more than {limit} bytes")]
    TooLarge { url: String, limit: u64 },
    #[error("{url} returned text that is not UTF-8")]
    NotUtf8 { url: String },
    #[error("failed to build the HTTP client")]
    Client(#[source] reqwest::Error),
}

/// Fetches NOAA documents with one shared client, a rate limit, transient
/// retries, status checks, and size limits.
pub struct XmlFetcher {
    logger: Logger,
    client: ClientWithMiddleware,
    rate_limiter: Arc<Mutex<RateLimiter>>,
}

impl XmlFetcher {
    pub fn new(
        logger: Logger,
        user_agent: &str,
        rate_limiter: Arc<Mutex<RateLimiter>>,
    ) -> Result<XmlFetcher, FetchError> {
        let client = Client::builder()
            .user_agent(user_agent)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(FetchError::Client)?;
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        Ok(Self {
            logger,
            client: ClientBuilder::new(client)
                .with(RetryTransientMiddleware::new_with_policy(retry_policy))
                .build(),
            rate_limiter,
        })
    }

    async fn get(&self, url: &str, timeout: Duration) -> Result<reqwest::Response, FetchError> {
        RateLimiter::acquire(&self.rate_limiter).await;
        debug!(self.logger, "requesting: {}", redact(url));
        let response = self
            .client
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|source| FetchError::Request {
                url: redact(url),
                source,
            })?;
        if !response.status().is_success() {
            return Err(FetchError::Status {
                url: redact(url),
                status: response.status(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES)
        {
            return Err(FetchError::TooLarge {
                url: redact(url),
                limit: MAX_RESPONSE_BYTES,
            });
        }
        Ok(response)
    }

    pub async fn fetch_xml(&self, url: &str) -> Result<String, FetchError> {
        let response = self.get(url, REQUEST_TIMEOUT).await?;
        let body = read_limited(
            response
                .bytes_stream()
                .map_err(std::io::Error::other)
                .into_async_read()
                .compat(),
            MAX_RESPONSE_BYTES,
            url,
        )
        .await?;
        String::from_utf8(body).map_err(|_| FetchError::NotUtf8 { url: redact(url) })
    }

    pub async fn fetch_xml_gzip(&self, url: &str) -> Result<String, FetchError> {
        let response = self.get(url, DOWNLOAD_TIMEOUT).await?;
        // Count compressed bytes as they are read, then bound the output.
        let mut compressed_seen: u64 = 0;
        let compressed = response
            .bytes_stream()
            .map_err(std::io::Error::other)
            .map(move |chunk| {
                let chunk = chunk?;
                compressed_seen += chunk.len() as u64;
                if compressed_seen > MAX_RESPONSE_BYTES {
                    return Err(std::io::Error::other("compressed body too large"));
                }
                Ok(chunk)
            })
            .into_async_read()
            .compat();
        let decoder = GzipDecoder::new(tokio::io::BufReader::new(compressed));
        let body = read_limited(decoder, MAX_DECOMPRESSED_BYTES, url).await?;
        String::from_utf8(body).map_err(|_| FetchError::NotUtf8 { url: redact(url) })
    }
}

/// Reads at most `limit` bytes; more is an error rather than truncation.
async fn read_limited(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: u64,
    url: &str,
) -> Result<Vec<u8>, FetchError> {
    let mut body = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut body)
        .await
        .map_err(|source| FetchError::Body {
            url: redact(url),
            source,
        })?;
    if body.len() as u64 > limit {
        return Err(FetchError::TooLarge {
            url: redact(url),
            limit,
        });
    }
    Ok(body)
}

/// The URL without credentials or query, for logs and errors.
pub fn redact(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.set_query(None);
            parsed.to_string()
        }
        Err(_) => String::from("<invalid url>"),
    }
}

/// Parses NOAA XML. Same-named siblings are not always adjacent in NOAA
/// documents (precipitation types interleave with wind elements), so
/// overlapping sequences are enabled.
pub fn parse_xml<'de, T: serde::Deserialize<'de>>(xml: &str) -> Result<T, serde_xml_rs::Error> {
    serde_xml_rs::SerdeXml::new()
        .overlapping_sequences(true)
        .from_str(xml)
}

#[cfg(test)]
mod tests {
    /// Clap checks its definition (duplicate short flags and the like) only
    /// when the binary starts, so check it here.
    #[test]
    fn the_command_line_definition_is_consistent() {
        use clap::CommandFactory;
        super::Cli::command().debug_assert();
    }

    use super::*;
    use async_compression::tokio::write::GzipEncoder;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn the_rate_limiter_caps_tokens_and_refills_over_the_period() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(3, Duration::from_secs(15));
        limiter.last_refill = start;
        for _ in 0..3 {
            assert!(limiter.try_take(start).is_ok());
        }
        let wait = limiter.try_take(start).unwrap_err();
        assert_eq!(wait, Duration::from_secs(5), "one token per 5 s");
        // A long idle period never banks more than the capacity.
        let later = start + Duration::from_secs(3600);
        for _ in 0..3 {
            assert!(limiter.try_take(later).is_ok());
        }
        assert!(limiter.try_take(later).is_err());
    }

    #[tokio::test]
    async fn oversized_and_gzip_bomb_bodies_are_rejected() {
        let small = read_limited(&b"12345"[..], 5, "https://x.test/a").await;
        assert_eq!(small.unwrap(), b"12345");
        assert!(matches!(
            read_limited(&b"123456"[..], 5, "https://x.test/a").await,
            Err(FetchError::TooLarge { limit: 5, .. })
        ));
        // 1 MiB of zeros compresses to about 1 KiB.
        let mut encoder = GzipEncoder::new(Vec::new());
        encoder.write_all(&vec![0u8; 1024 * 1024]).await.unwrap();
        encoder.shutdown().await.unwrap();
        let compressed = encoder.into_inner();
        assert!(compressed.len() < 4096);
        let decoder = GzipDecoder::new(tokio::io::BufReader::new(&compressed[..]));
        assert!(matches!(
            read_limited(decoder, 64 * 1024, "https://x.test/a").await,
            Err(FetchError::TooLarge { .. })
        ));
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let with = |change: fn(&mut Cli)| {
            let mut cli = Cli::default();
            change(&mut cli);
            cli.configuration()
        };
        assert!(with(|_| {}).is_ok());
        assert!(matches!(
            with(|cli| cli.sleep_interval = Some(0)),
            Err(ConfigError::NotPositive("sleep_interval"))
        ));
        assert!(matches!(
            with(|cli| cli.token_capacity = Some(0)),
            Err(ConfigError::NotPositive("token_capacity"))
        ));
        assert!(matches!(
            with(|cli| cli.refill_rate = Some(0.0)),
            Err(ConfigError::NotPositive("refill_rate"))
        ));
        assert!(matches!(
            with(|cli| cli.min_forecast_coverage = Some(1.5)),
            Err(ConfigError::Coverage)
        ));
        assert!(matches!(
            with(|cli| cli.base_url = Some("http://u:p@oracle.test".into())),
            Err(ConfigError::BaseUrl(_))
        ));
        assert!(matches!(
            with(|cli| cli.level = Some("loud".into())),
            Err(ConfigError::Level(_))
        ));
        assert!(toml::from_str::<Cli>("base_urll = \"x\"").is_err());
    }

    #[test]
    fn logged_urls_drop_credentials_and_queries() {
        assert_eq!(
            redact("https://user:secret@example.com/path?token=abc"),
            "https://example.com/path"
        );
    }
}
