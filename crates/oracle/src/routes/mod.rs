//! HTTP routes. Handlers use [`crate::AppState`] capabilities and map errors
//! to status codes; they never touch database connections directly.

use crate::weather_data::{MAX_DAYS, PRECOMPUTED_DAYS};
use axum::{extract::FromRequestParts, http::request::Parts};

pub mod discovery;
pub mod events;
pub mod files;
pub mod health;
pub mod stations;
pub mod ui;
pub mod window_compatibility;

pub use events::{
    Base64Pubkey, Pubkey, add_event_entries, create_event, current_lines, get_event,
    get_event_entry, get_npub, get_pubkey, list_events, list_sources, update_data,
};
pub use files::{Files, download, files, upload};
pub use health::{health, healthy, ready};
pub use stations::{
    ForecastRequest, ObservationRequest, TemperatureUnit, daily_observations, forecasts,
    get_stations, observations,
};
pub use ui::{
    dashboard_handler, event_detail_handler, events_handler, forecast_handler, raw_data_handler,
    station_handler, warm_caches, weather_handler,
};

/// Who a request came from: the private listener, which installs
/// [`ui::OperatorView`] and sits behind the operator access policy, or the
/// public one. Public requests get the bounds anyone may use; operators may
/// ask for more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    Public,
    Operator,
}

impl Audience {
    /// Most days of history an eligible station list may judge: for the
    /// public only the days read ahead after each collection run, so a
    /// public request never reads weeks of reports.
    pub fn max_eligible_days(self) -> u32 {
        match self {
            Audience::Public => PRECOMPUTED_DAYS,
            Audience::Operator => MAX_DAYS,
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Audience {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(if parts.extensions.get::<ui::OperatorView>().is_some() {
            Audience::Operator
        } else {
            Audience::Public
        })
    }
}
