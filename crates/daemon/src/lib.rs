//! NOAA data daemon: collects source data as parquet, publishes it to the
//! oracle and S3, and keeps the local copy until it is published.

mod coordinates;
mod domains;
pub mod keys;
pub mod publish;
mod s3_storage;
pub mod source;
mod utils;

pub use coordinates::{CityWeather, WeatherStation, get_coordinates, split_cityweather};
pub use domains::*;
pub use s3_storage::S3Storage;
pub use utils::{
    Cli, ConfigError, Configuration, FetchError, RateLimiter, S3Settings, XmlFetcher, parse_xml,
    redact, setup_logger,
};
