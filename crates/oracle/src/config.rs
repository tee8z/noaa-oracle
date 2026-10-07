//! Configuration from CLI flags, environment variables, and a TOML file,
//! validated into [`Configuration`] before anything is opened, plus
//! process logging setup.

use crate::request_context::{Cidr, DEFAULT_CLIENT_IP_HEADER, RequestSettings};
use axum::http::HeaderName;
use clap::Parser;
use fern::{
    Dispatch,
    colors::{Color, ColoredLevelConfig},
};
use log::LevelFilter;
use noaa_oracle_core::{ConfigSource, DEFAULT_ORACLE_PORT, find_config_file, load_config};
use nostr::key::PublicKey;
use std::{
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Iso8601};
use tracing_appender::non_blocking::WorkerGuard;

const DEFAULT_SHUTDOWN_TIMEOUT_SECONDS: u64 = 25;
const DEFAULT_ETL_INTERVAL_SECONDS: u64 = 300;

/// Settings as given. Every field is optional so the file and the command
/// line can be merged; [`Cli::configuration`] applies defaults and checks.
#[derive(Parser, Clone, Debug, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
    pub config: Option<PathBuf>,

    /// Log level: trace, debug, info, warn, error
    #[arg(short, long, env = "NOAA_ORACLE_LEVEL")]
    pub level: Option<LevelFilter>,

    /// Address to listen on (use 0.0.0.0 for all interfaces)
    #[arg(long, env = "NOAA_ORACLE_HOST")]
    #[serde(alias = "domain")]
    pub host: Option<IpAddr>,

    /// Port to listen on
    #[arg(short, long, env = "NOAA_ORACLE_PORT")]
    pub port: Option<u16>,

    /// Address for a separate Prometheus metrics listener, e.g.
    /// 127.0.0.1:9801. Also serves operator pages, without authentication.
    /// Bind privately and enforce operator access at the proxy. Unset disables it.
    #[arg(long, env = "NOAA_ORACLE_METRICS_BIND")]
    pub metrics_bind: Option<SocketAddr>,

    /// Public URL clients use to reach the oracle, e.g. https://example.com.
    /// NIP-98 signatures must be made over this origin.
    #[arg(short, long, env = "NOAA_ORACLE_REMOTE_URL")]
    pub remote_url: Option<String>,

    /// Directory containing weather parquet files
    #[arg(short, long, env = "NOAA_ORACLE_DATA_DIR")]
    #[serde(alias = "data_dir")]
    pub weather_dir: Option<PathBuf>,

    /// Directory for the event database
    #[arg(short, long, env = "NOAA_ORACLE_EVENT_DB")]
    pub event_db: Option<PathBuf>,

    /// Ignored: the binary embeds its UI. Accepted so older settings still load.
    #[arg(short, long, env = "NOAA_ORACLE_UI_DIR", hide = true)]
    pub ui_dir: Option<PathBuf>,

    /// Path to the oracle signing key (secp256k1, PEM, mode 0600)
    #[arg(short, long, env = "NOAA_ORACLE_PRIVATE_KEY")]
    #[serde(alias = "private_key_path")]
    pub oracle_private_key: Option<PathBuf>,

    /// Nostr pubkeys (npub or hex) allowed to create events and submit entries
    #[arg(long, env = "NOAA_ORACLE_COORDINATOR_PUBKEYS", value_delimiter = ',')]
    #[serde(default)]
    pub coordinator_pubkeys: Vec<String>,

    /// Nostr pubkeys (npub or hex) allowed to upload data files
    #[arg(long, env = "NOAA_ORACLE_UPLOADER_PUBKEYS", value_delimiter = ',')]
    #[serde(default)]
    pub uploader_pubkeys: Vec<String>,

    /// S3 bucket name for listing and serving weather data files
    #[arg(long, env = "NOAA_ORACLE_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    /// Custom S3 endpoint URL (for MinIO or other S3-compatible storage)
    #[arg(long, env = "NOAA_ORACLE_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    /// Seconds between scheduled scoring and signing passes
    #[arg(long, env = "NOAA_ORACLE_ETL_INTERVAL")]
    pub etl_interval: Option<u64>,

    /// Seconds to wait for HTTP requests, background work, and accepted
    /// writes to finish after a stop signal before giving up
    #[arg(long, env = "NOAA_ORACLE_SHUTDOWN_TIMEOUT")]
    pub shutdown_timeout: Option<u64>,

    /// Publishing announcements and attestations to Nostr relays
    #[command(flatten)]
    #[serde(default)]
    pub nostr: NostrArgs,

    /// Request ids and client addresses from the proxy in front
    #[command(flatten)]
    #[serde(default)]
    pub http_context: HttpContextArgs,

    /// The browser telemetry beacon
    #[command(flatten)]
    #[serde(default)]
    pub telemetry: TelemetryArgs,
}

/// The `[nostr]` table: publishing to Nostr relays (see `docs/NOSTR.md`).
#[derive(clap::Args, Clone, Debug, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NostrArgs {
    /// Publish event announcements and attestations to the relays
    #[arg(
        id = "nostr_enabled",
        long = "nostr-enabled",
        env = "NOAA_ORACLE_NOSTR_ENABLED"
    )]
    pub enabled: Option<bool>,

    /// Relay URLs (wss:// or ws://)
    #[arg(
        id = "nostr_relays",
        long = "nostr-relays",
        env = "NOAA_ORACLE_NOSTR_RELAYS",
        value_delimiter = ','
    )]
    #[serde(default)]
    pub relays: Vec<String>,

    /// Path to the Nostr publishing key (secp256k1, PEM, mode 0600). Must
    /// not be the oracle signing key.
    #[arg(
        id = "nostr_key_path",
        long = "nostr-key",
        env = "NOAA_ORACLE_NOSTR_KEY"
    )]
    pub key_path: Option<PathBuf>,
}

/// The `[http_context]` table: which peers may tell the oracle the request
/// id and the client's address (see `docs/request-logging.md`).
#[derive(clap::Args, Clone, Debug, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HttpContextArgs {
    /// Proxies (addresses or CIDRs) whose X-Request-Id and client address
    /// header are believed. Empty: none, so the peer address is logged.
    #[arg(
        id = "trusted_proxies",
        long = "trusted-proxies",
        env = "NOAA_ORACLE_TRUSTED_PROXIES",
        value_delimiter = ','
    )]
    #[serde(default)]
    pub trusted_proxies: Vec<String>,

    /// Header a trusted proxy puts the client's address in (default X-Real-IP)
    #[arg(
        id = "client_ip_header",
        long = "client-ip-header",
        env = "NOAA_ORACLE_CLIENT_IP_HEADER"
    )]
    pub client_ip_header: Option<String>,
}

/// The `[telemetry]` table: the browser beacon and `POST /api/v1/telemetry`.
#[derive(clap::Args, Clone, Debug, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TelemetryArgs {
    /// Load the beacon on pages and log its events
    #[arg(
        id = "telemetry_enabled",
        long = "telemetry-enabled",
        env = "NOAA_ORACLE_TELEMETRY_ENABLED"
    )]
    pub enabled: Option<bool>,
}

/// Validated settings the application starts from.
#[derive(Clone, Debug)]
pub struct Configuration {
    pub listen: SocketAddr,
    /// Where metrics are served; `None` disables the metrics listener.
    pub metrics_listen: Option<SocketAddr>,
    pub remote_url: String,
    pub weather_dir: PathBuf,
    pub event_dir: PathBuf,
    pub private_key: PathBuf,
    pub coordinators: Vec<PublicKey>,
    pub uploaders: Vec<PublicKey>,
    pub storage: Storage,
    pub etl_interval: Duration,
    pub shutdown_timeout: Duration,
    /// Relay publishing; `None` when off.
    pub nostr: Option<NostrSettings>,
    /// Trusted proxies, the client address header, and telemetry.
    pub request: RequestSettings,
}

/// Validated `[nostr]` settings for an enabled publisher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NostrSettings {
    pub relays: Vec<String>,
    pub key_path: PathBuf,
}

/// Where data files are listed and served from. Uploads and queries always
/// use the local weather directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Storage {
    Local,
    S3 {
        bucket: String,
        endpoint: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}")]
    File {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
    #[error("invalid remote_url {0:?}: expected http(s)://host[:port] without a path")]
    RemoteUrl(String),
    #[error("invalid {field} entry {value:?}: expected an npub or hex public key")]
    Pubkey { field: &'static str, value: String },
    #[error("{0} must be positive")]
    Zero(&'static str),
    #[error("invalid nostr relay {0:?}: expected a wss:// or ws:// URL")]
    Relay(String),
    #[error("nostr is enabled without relays")]
    NoRelays,
    #[error("the nostr key must be a separate file from the oracle signing key")]
    SharedKey,
    #[error("invalid trusted_proxies entry {0:?}: expected an IP address or CIDR")]
    TrustedProxy(String),
    #[error("invalid client_ip_header {0:?}: expected an HTTP header name")]
    ClientIpHeader(String),
}

impl Cli {
    /// Parses flags and merges them over the config file. Flags and
    /// environment variables win over the file.
    pub fn load() -> Result<Self, ConfigError> {
        let cli = Cli::parse();
        let source = match &cli.config {
            Some(path) => ConfigSource::Explicit(path.clone()),
            None => find_config_file("NOAA_ORACLE_CONFIG", "oracle.toml"),
        };
        let file: Cli = match source.path() {
            Some(path) => load_config(&source).map_err(|source| ConfigError::File {
                path: path.clone(),
                source,
            })?,
            None => Cli::default(),
        };
        Ok(cli.merge(file))
    }

    fn merge(self, file: Cli) -> Cli {
        let list = |cli: Vec<String>, file: Vec<String>| if cli.is_empty() { file } else { cli };
        Cli {
            config: self.config,
            level: self.level.or(file.level),
            host: self.host.or(file.host),
            port: self.port.or(file.port),
            metrics_bind: self.metrics_bind.or(file.metrics_bind),
            remote_url: self.remote_url.or(file.remote_url),
            weather_dir: self.weather_dir.or(file.weather_dir),
            event_db: self.event_db.or(file.event_db),
            ui_dir: self.ui_dir.or(file.ui_dir),
            oracle_private_key: self.oracle_private_key.or(file.oracle_private_key),
            coordinator_pubkeys: list(self.coordinator_pubkeys, file.coordinator_pubkeys),
            uploader_pubkeys: list(self.uploader_pubkeys, file.uploader_pubkeys),
            s3_bucket: self.s3_bucket.or(file.s3_bucket),
            s3_endpoint: self.s3_endpoint.or(file.s3_endpoint),
            etl_interval: self.etl_interval.or(file.etl_interval),
            shutdown_timeout: self.shutdown_timeout.or(file.shutdown_timeout),
            nostr: NostrArgs {
                enabled: self.nostr.enabled.or(file.nostr.enabled),
                relays: list(self.nostr.relays, file.nostr.relays),
                key_path: self.nostr.key_path.or(file.nostr.key_path),
            },
            http_context: HttpContextArgs {
                trusted_proxies: list(
                    self.http_context.trusted_proxies,
                    file.http_context.trusted_proxies,
                ),
                client_ip_header: self
                    .http_context
                    .client_ip_header
                    .or(file.http_context.client_ip_header),
            },
            telemetry: TelemetryArgs {
                enabled: self.telemetry.enabled.or(file.telemetry.enabled),
            },
        }
    }

    pub fn log_level(&self) -> LevelFilter {
        self.level.unwrap_or(LevelFilter::Info)
    }

    /// Applies defaults and validates every setting before anything opens.
    pub fn configuration(&self) -> Result<Configuration, ConfigError> {
        let host = self.host.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let port = self.port.unwrap_or(DEFAULT_ORACLE_PORT);
        let remote_url = match &self.remote_url {
            Some(url) => validate_origin(url)?,
            None => format!("http://{}", SocketAddr::new(host, port)),
        };
        let private_key = self
            .oracle_private_key
            .clone()
            .unwrap_or_else(|| PathBuf::from("./oracle_private_key.pem"));
        let nostr = self.nostr.settings(&private_key)?;
        let seconds = |value: Option<u64>, default, name| match value.unwrap_or(default) {
            0 => Err(ConfigError::Zero(name)),
            seconds => Ok(Duration::from_secs(seconds)),
        };
        Ok(Configuration {
            listen: SocketAddr::new(host, port),
            metrics_listen: self.metrics_bind,
            remote_url,
            weather_dir: self
                .weather_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./weather_data")),
            event_dir: self
                .event_db
                .clone()
                .unwrap_or_else(|| PathBuf::from("./event_data")),
            private_key,
            coordinators: parse_pubkeys("coordinator_pubkeys", &self.coordinator_pubkeys)?,
            uploaders: parse_pubkeys("uploader_pubkeys", &self.uploader_pubkeys)?,
            storage: match self.s3_bucket.clone().filter(|bucket| !bucket.is_empty()) {
                Some(bucket) => Storage::S3 {
                    bucket,
                    endpoint: self
                        .s3_endpoint
                        .clone()
                        .filter(|endpoint| !endpoint.is_empty()),
                },
                None => Storage::Local,
            },
            etl_interval: seconds(
                self.etl_interval,
                DEFAULT_ETL_INTERVAL_SECONDS,
                "etl_interval",
            )?,
            shutdown_timeout: seconds(
                self.shutdown_timeout,
                DEFAULT_SHUTDOWN_TIMEOUT_SECONDS,
                "shutdown_timeout",
            )?,
            nostr,
            request: self.request_settings()?,
        })
    }

    fn request_settings(&self) -> Result<RequestSettings, ConfigError> {
        let trusted_proxies = self
            .http_context
            .trusted_proxies
            .iter()
            .map(|proxy| proxy.trim())
            .filter(|proxy| !proxy.is_empty())
            .map(|proxy| {
                proxy
                    .parse::<Cidr>()
                    .map_err(|_| ConfigError::TrustedProxy(proxy.to_owned()))
            })
            .collect::<Result<_, _>>()?;
        let header = self
            .http_context
            .client_ip_header
            .as_deref()
            .map(str::trim)
            .filter(|header| !header.is_empty())
            .unwrap_or(DEFAULT_CLIENT_IP_HEADER);
        let client_ip_header = HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| ConfigError::ClientIpHeader(header.to_owned()))?;
        Ok(RequestSettings {
            trusted_proxies,
            client_ip_header,
            telemetry: self.telemetry.enabled == Some(true),
        })
    }
}

impl NostrArgs {
    /// `None` unless enabled. An enabled publisher needs relays and a key
    /// file other than `private_key`, the oracle signing key.
    fn settings(
        &self,
        private_key: &std::path::Path,
    ) -> Result<Option<NostrSettings>, ConfigError> {
        if self.enabled != Some(true) {
            return Ok(None);
        }
        let relays: Vec<String> = self
            .relays
            .iter()
            .map(|relay| relay.trim())
            .filter(|relay| !relay.is_empty())
            .map(validate_relay)
            .collect::<Result<_, _>>()?;
        if relays.is_empty() {
            return Err(ConfigError::NoRelays);
        }
        let key_path = self
            .key_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("./oracle_nostr_key.pem"));
        if key_path.as_path() == private_key {
            return Err(ConfigError::SharedKey);
        }
        Ok(Some(NostrSettings { relays, key_path }))
    }
}

/// Accepts `wss://` and `ws://` URLs and returns them without a trailing
/// slash, so one relay is one outbox key however it is written.
fn validate_relay(relay: &str) -> Result<String, ConfigError> {
    let invalid = || ConfigError::Relay(relay.to_owned());
    let parsed = nostr::types::Url::parse(relay).map_err(|_| invalid())?;
    if matches!(parsed.scheme(), "wss" | "ws") && parsed.host_str().is_some() {
        Ok(relay.trim_end_matches('/').to_owned())
    } else {
        Err(invalid())
    }
}

/// Accepts `http(s)://host[:port]` and returns it without a trailing slash.
fn validate_origin(url: &str) -> Result<String, ConfigError> {
    let invalid = || ConfigError::RemoteUrl(url.to_owned());
    let trimmed = url.trim_end_matches('/');
    let parsed = nostr::types::Url::parse(trimmed).map_err(|_| invalid())?;
    let origin_only = matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none();
    if origin_only {
        Ok(trimmed.to_owned())
    } else {
        Err(invalid())
    }
}

fn parse_pubkeys(field: &'static str, values: &[String]) -> Result<Vec<PublicKey>, ConfigError> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            PublicKey::parse(value).map_err(|_| ConfigError::Pubkey {
                field,
                value: value.to_owned(),
            })
        })
        .collect()
}

/// Process logging. Lines go to stdout from a background thread, so a
/// request never waits on stdout: a slow journald, a full pipe or a disk
/// busy with other writes. If that thread falls 128,000 lines behind, new
/// lines are dropped rather than waited for. Keep the guard until the
/// process ends; dropping it writes out the lines still queued.
///
/// Lines logged while a request is handled end with ` rid=<id>` (see
/// [`crate::request_context`]).
pub fn setup_logger() -> (Dispatch, WorkerGuard) {
    let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
    let colors = ColoredLevelConfig::new()
        .trace(Color::White)
        .debug(Color::Cyan)
        .info(Color::Blue)
        .warn(Color::Yellow)
        .error(Color::Magenta);

    let dispatch = fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "[{} {}] {}: {}{}",
                OffsetDateTime::now_utc()
                    .format(&Iso8601::DEFAULT)
                    .unwrap_or_default(),
                colors.color(record.level()),
                record.target(),
                message,
                crate::request_context::log_suffix(record.target())
            ));
        })
        .chain(fern::Output::call(move |record| {
            // One write per line, so a dropped line is never half written.
            let line = format!("{}\n", record.args());
            let _ = writer.clone().write_all(line.as_bytes());
        }));
    (dispatch, guard)
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

    #[test]
    fn defaults_produce_a_local_listener() {
        let configuration = Cli::default().configuration().unwrap();
        assert_eq!(configuration.listen.to_string(), "127.0.0.1:9800");
        assert_eq!(configuration.remote_url, "http://127.0.0.1:9800");
        assert_eq!(configuration.shutdown_timeout, Duration::from_secs(25));
        assert_eq!(configuration.storage, Storage::Local);
        assert!(configuration.coordinators.is_empty());
        assert_eq!(configuration.metrics_listen, None, "metrics are off");
    }

    #[test]
    fn metrics_bind_enables_the_metrics_listener() {
        let parsed: Cli = toml::from_str("metrics_bind = \"127.0.0.1:9801\"").unwrap();
        let configuration = parsed.configuration().unwrap();
        assert_eq!(
            configuration.metrics_listen,
            Some("127.0.0.1:9801".parse().unwrap())
        );
        assert_eq!(configuration.listen.to_string(), "127.0.0.1:9800");

        let flag = Cli::try_parse_from(["oracle", "--metrics-bind", "0.0.0.0:9900"]).unwrap();
        assert_eq!(
            flag.merge(parsed).configuration().unwrap().metrics_listen,
            Some("0.0.0.0:9900".parse().unwrap()),
            "the flag wins over the file"
        );
        assert!(toml::from_str::<Cli>("metrics_bind = \"not an address\"").is_err());
    }

    #[test]
    fn invalid_values_are_rejected_before_startup() {
        let with = |change: fn(&mut Cli)| {
            let mut cli = Cli::default();
            change(&mut cli);
            cli.configuration()
        };
        assert!(matches!(
            with(|cli| cli.shutdown_timeout = Some(0)),
            Err(ConfigError::Zero("shutdown_timeout"))
        ));
        assert!(matches!(
            with(|cli| cli.etl_interval = Some(0)),
            Err(ConfigError::Zero("etl_interval"))
        ));
        assert!(matches!(
            with(|cli| cli.coordinator_pubkeys = vec!["nope".into()]),
            Err(ConfigError::Pubkey { .. })
        ));
        for url in [
            "ftp://example.com",
            "https://example.com/api",
            "https://u:p@example.com",
        ] {
            let cli = Cli {
                remote_url: Some(url.into()),
                ..Cli::default()
            };
            assert!(
                matches!(cli.configuration(), Err(ConfigError::RemoteUrl(_))),
                "{url}"
            );
        }
    }

    #[test]
    fn pubkeys_accept_npub_and_hex() {
        let keys = nostr::key::Keys::generate();
        let hex = keys.public_key().to_hex();
        let npub = nostr::nips::nip19::ToBech32::to_bech32(&keys.public_key()).unwrap();
        let cli = Cli {
            coordinator_pubkeys: vec![npub],
            uploader_pubkeys: vec![hex],
            remote_url: Some("https://oracle.example.com/".into()),
            ..Cli::default()
        };
        let configuration = cli.configuration().unwrap();
        assert_eq!(configuration.coordinators, vec![keys.public_key()]);
        assert_eq!(configuration.uploaders, vec![keys.public_key()]);
        assert_eq!(configuration.remote_url, "https://oracle.example.com");
    }

    #[test]
    fn nostr_publishing_is_off_unless_enabled_with_relays() {
        assert_eq!(Cli::default().configuration().unwrap().nostr, None);
        let parsed: Cli =
            toml::from_str("[nostr]\nenabled = false\nrelays = [\"wss://relay.example.com\"]")
                .unwrap();
        assert_eq!(parsed.configuration().unwrap().nostr, None);

        let parsed: Cli = toml::from_str(
            "port = 9900\n[nostr]\nenabled = true\nrelays = [\"wss://relay.example.com/\", \"ws://127.0.0.1:7777\"]",
        )
        .unwrap();
        assert_eq!(
            parsed.configuration().unwrap().nostr,
            Some(NostrSettings {
                relays: vec![
                    "wss://relay.example.com".into(),
                    "ws://127.0.0.1:7777".into()
                ],
                key_path: PathBuf::from("./oracle_nostr_key.pem"),
            })
        );

        let flag =
            Cli::try_parse_from(["oracle", "--nostr-relays", "wss://other.example.com"]).unwrap();
        assert_eq!(
            flag.merge(parsed)
                .configuration()
                .unwrap()
                .nostr
                .unwrap()
                .relays,
            vec!["wss://other.example.com".to_owned()],
            "the flag wins over the file"
        );

        let enabled = |relays: &str, key: &str| {
            toml::from_str::<Cli>(&format!(
                "private_key_path = \"./oracle.pem\"\n[nostr]\nenabled = true\nrelays = {relays}\n{key}"
            ))
            .unwrap()
            .configuration()
        };
        assert!(matches!(enabled("[]", ""), Err(ConfigError::NoRelays)));
        assert!(matches!(
            enabled("[\"https://relay.example.com\"]", ""),
            Err(ConfigError::Relay(_))
        ));
        assert!(matches!(
            enabled(
                "[\"wss://relay.example.com\"]",
                "key_path = \"./oracle.pem\""
            ),
            Err(ConfigError::SharedKey)
        ));
        assert!(toml::from_str::<Cli>("[nostr]\nrelay = []").is_err());
    }

    #[test]
    fn request_context_settings_default_to_no_proxies_and_no_telemetry() {
        let request = Cli::default().configuration().unwrap().request;
        assert!(request.trusted_proxies.is_empty());
        assert_eq!(request.client_ip_header, "x-real-ip");
        assert!(!request.telemetry);

        let parsed: Cli = toml::from_str(
            "[http_context]\ntrusted_proxies = [\"127.0.0.1\", \"10.0.0.0/8\"]\n\
             client_ip_header = \"X-Client-IP\"\n[telemetry]\nenabled = true",
        )
        .unwrap();
        let request = parsed.configuration().unwrap().request;
        assert_eq!(request.trusted_proxies.len(), 2);
        assert_eq!(request.client_ip_header, "x-client-ip");
        assert!(request.telemetry);

        let flag = Cli::try_parse_from([
            "oracle",
            "--trusted-proxies",
            "::1,127.0.0.1",
            "--telemetry-enabled",
            "false",
        ])
        .unwrap();
        let request = flag.merge(parsed).configuration().unwrap().request;
        assert_eq!(request.trusted_proxies.len(), 2);
        assert!(!request.telemetry, "the flag wins over the file");

        let invalid = |toml: &str| toml::from_str::<Cli>(toml).unwrap().configuration();
        assert!(matches!(
            invalid("[http_context]\ntrusted_proxies = [\"10.0.0.0/40\"]"),
            Err(ConfigError::TrustedProxy(_))
        ));
        assert!(matches!(
            invalid("[http_context]\nclient_ip_header = \"Not A Header\""),
            Err(ConfigError::ClientIpHeader(_))
        ));
        assert!(toml::from_str::<Cli>("[telemetry]\nenable = true").is_err());
    }

    #[test]
    fn unknown_config_file_keys_are_rejected() {
        assert!(toml::from_str::<Cli>("hosst = \"0.0.0.0\"").is_err());
        let parsed: Cli = toml::from_str("host = \"0.0.0.0\"\nport = 9900").unwrap();
        assert_eq!(parsed.port, Some(9900));
    }
}
