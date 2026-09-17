//! NOAA Oracle: serves weather data from parquet files, tracks DLC weather
//! events in SQLite, and signs attestations.
//!
//! The library exposes what the binary and the integration tests need:
//! configuration, the router, the event types on the wire, and the
//! capabilities that back them.

mod app_error;
pub mod config;
pub mod database;
pub mod events;
pub mod file_access;
pub mod nostr_extractor;
pub mod oracle;
pub mod routes;
mod startup;
mod templates;
pub mod weather_data;

pub use app_error::AppError;
pub use config::{Cli, Configuration, get_config_info, get_log_level, setup_logger};
pub use database::{Database, DatabaseWriter, WriteError};
pub use events::{
    ActiveEvent, AddEventEntries, AddEventEntry, CreateEvent, CreateEventData, Event, EventFilter,
    EventStatus, EventSummary, Forecasted, Observed, ScoringField, SignEvent, ValueOptions,
    Weather, WeatherChoices, WeatherEntry,
};
pub use file_access::{FileAccess, FileData, FileParams, ParquetFileName, S3FileAccess};
pub use nostr_extractor::{AuthError, NostrAuth};
pub use routes::{ForecastRequest, ObservationRequest, TemperatureUnit};
pub use startup::{AppState, Background, EtlRejected, app, run_until_stop};
pub use weather_data::{
    DailyObservation, Forecast, Observation, Station, WeatherAccess, WeatherData,
};
