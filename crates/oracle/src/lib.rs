//! NOAA Oracle: serves weather data from parquet files, tracks DLC events
//! in SQLite, and attests outcomes.
//!
//! The library exposes what the binary and the integration tests need:
//! configuration, the router, the event types on the wire, and the
//! capabilities that back them.

mod app_error;
pub mod auth;
pub mod calendar;
pub mod config;
pub mod database;
pub mod events;
pub mod file_access;
pub mod oracle;
pub mod routes;
pub mod scoring;
pub mod signing;
pub mod sources;
mod startup;
mod templates;
pub mod weather_data;

pub use app_error::AppError;
pub use config::{Cli, Configuration, setup_logger};
pub use database::Database;
pub use events::{
    AddEventEntries, AddEventEntry, CreateEvent, Event, EventStatus, EventSummary, ScoringField,
    ValueOptions, Weather, WeatherChoices, WeatherEntry,
};
pub use file_access::{FileData, FileParams, ParquetFileName};
pub use routes::{ForecastRequest, ObservationRequest, TemperatureUnit};
pub use startup::{AppParts, AppState, Background, EtlRejected, app, run_until_stop};
pub use weather_data::{DailyObservation, Forecast, Observation, Station, WeatherData};
