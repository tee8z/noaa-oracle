use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use super::htmx::{Render, page_or_fragment};
use crate::{
    AppState,
    routes::{ObservationRequest, TemperatureUnit},
    sources::noaa::NOAA_WEATHER,
    templates::pages::event_detail::{
        SettlementQuality, event_detail_fragment, event_detail_page, event_not_found_fragment,
        event_not_found_page, event_unavailable_fragment, event_unavailable_page,
    },
};

/// Handler for the event detail page (GET /events/{id}).
/// Returns the full page for normal requests and only the content for htmx,
/// which swaps it into the page's existing layout.
pub async fn event_detail_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<Uuid>,
) -> Response {
    match state.oracle.get_event(event_id).await {
        Ok(event) => {
            let now = time::OffsetDateTime::now_utc();
            let quality = if event.attestation.is_some() {
                SettlementQuality::Signed
            } else if event.settlement_block.is_some() {
                // The page renders the persisted processing failure. A fresh
                // observation-only check cannot clear forecast or coverage holds.
                SettlementQuality::Unavailable
            } else if event.source == NOAA_WEATHER.as_str() {
                let request = ObservationRequest {
                    start: Some(event.start_observation_date),
                    end: Some(event.end_observation_date - time::Duration::nanoseconds(1)),
                    station_ids: event.locations.join(","),
                    temperature_unit: TemperatureUnit::Fahrenheit,
                };
                match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    state.weather_db.observation_quality(
                        &request,
                        event.locations.clone(),
                        &event.scoring_fields,
                    ),
                )
                .await
                {
                    Ok(Ok(quality))
                        if quality.rejected_reports == 0 && quality.unverified_reports == 0 =>
                    {
                        SettlementQuality::Clear
                    }
                    Ok(Ok(quality)) => SettlementQuality::Blocked {
                        rejected_reports: quality.rejected_reports,
                        unverified_reports: quality.unverified_reports,
                    },
                    Ok(Err(error)) => {
                        log::warn!(
                            "event page {event_id}: observation quality unavailable: {error:#}"
                        );
                        SettlementQuality::Unavailable
                    }
                    Err(_) => {
                        log::warn!("event page {event_id}: observation quality check timed out");
                        SettlementQuality::Unavailable
                    }
                }
            } else {
                SettlementQuality::Unavailable
            };
            page_or_fragment(
                match super::htmx::render(&headers) {
                    Render::Page => event_detail_page(&event, now, quality),
                    _ => event_detail_fragment(&event, now, quality),
                }
                .into_string(),
            )
        }
        Err(crate::oracle::Error::EventNotFound(_)) => {
            let html = match super::htmx::render(&headers) {
                Render::Page => event_not_found_page(event_id),
                _ => event_not_found_fragment(event_id),
            };
            (StatusCode::NOT_FOUND, page_or_fragment(html.into_string())).into_response()
        }
        Err(error) => {
            log::error!("event page {event_id}: {error:#}");
            let html = match super::htmx::render(&headers) {
                Render::Page => event_unavailable_page(event_id),
                _ => event_unavailable_fragment(event_id),
            };
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                page_or_fragment(html.into_string()),
            )
                .into_response()
        }
    }
}
