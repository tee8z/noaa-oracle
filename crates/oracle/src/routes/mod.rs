//! HTTP routes. Handlers use [`crate::AppState`] capabilities and map errors
//! to status codes; they never touch database connections directly.

pub mod events;
pub mod files;
pub mod health;
pub mod stations;
pub mod ui;

pub use events::{
    Base64Pubkey, Pubkey, add_event_entries, create_event, get_event, get_event_entry, get_npub,
    get_pubkey, list_events, list_sources, update_data,
};
pub use files::{Files, download, files, upload};
pub use health::{healthy, ready};
pub use stations::{
    ForecastRequest, ObservationRequest, TemperatureUnit, daily_observations, forecasts,
    get_stations, observations,
};
pub use ui::{
    dashboard_handler, event_detail_handler, event_stats_handler, events_cards_handler,
    events_handler, events_rows_handler, forecast_handler, oracle_info_handler, raw_data_handler,
    warm_forecast_cache, weather_handler,
};
