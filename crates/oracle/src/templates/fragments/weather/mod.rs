//! Current weather: a map and a list of the same stations, rendered on the
//! server. The Map/List tabs, station search and refresh are htmx requests
//! to `/fragments/weather`, which returns this section or, for a search,
//! only the list.

mod list;
mod map;

use maud::{Markup, html};
use time::{OffsetDateTime, Time};

use crate::{
    templates::components::{time as when, values::Settled},
    weather_data::Station,
};

pub use list::weather_list;

#[derive(Clone)]
pub enum ObservationPeriod {
    Today,
    Selected { start: String, end: String },
}

impl ObservationPeriod {
    /// Today is a UTC day. A selection's times are shown in the reader's
    /// time zone (`local_time.js`), with UTC on hover, so its label names
    /// no zone.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Today => "Today so far (UTC)",
            Self::Selected { .. } => "Selected period",
        }
    }

    /// Whether the differences are final: only for a selection of whole UTC
    /// days that has ended. Today, a selection still running and part of a
    /// day all compare some hours of reports with a whole day's forecast.
    pub fn settled(&self, now: OffsetDateTime) -> Settled {
        let Self::Selected { start, end } = self else {
            return Settled::SoFar;
        };
        match (when::parse(start), when::parse(end)) {
            (Some(start), Some(end))
                if start.time() == Time::MIDNIGHT
                    && end.time() == Time::MIDNIGHT
                    && start < end
                    && end <= now =>
            {
                Settled::Final
            }
            _ => Settled::SoFar,
        }
    }
}

/// Weather data for display
pub struct WeatherDisplay {
    pub station_id: String,
    pub station_name: String,
    pub state: String,
    pub iata_id: String,
    /// Most recent temperature report, independent of the aggregate window.
    pub latest_temp: Option<f64>,
    pub latest_temp_time: Option<String>,
    pub observation_period: ObservationPeriod,
    pub temp_high: Option<f64>,
    pub temp_low: Option<f64>,
    pub wind_speed: Option<i64>,
    pub wind_direction: Option<i64>,
    pub humidity: Option<i64>,
    pub rain_amt: Option<f64>,
    pub snow_amt: Option<f64>,
    pub observed_start: String,
    pub observed_end: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Yesterday's forecast high for today (what was predicted)
    pub forecast_high: Option<i64>,
    /// Yesterday's forecast low for today (what was predicted)
    pub forecast_low: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WeatherView {
    #[default]
    Map,
    List,
}

impl WeatherView {
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value? {
            "map" => Some(Self::Map),
            "list" => Some(Self::List),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Map => "map",
            Self::List => "list",
        }
    }
}

/// What the reader asked for, besides the data itself.
pub struct WeatherContext<'a> {
    pub view: WeatherView,
    /// Station search text (list view).
    pub query: &'a str,
    /// `/fragments/weather` with the station and date selection only.
    pub selection_path: &'a str,
    /// Every station in the data, so a search can offer ones not shown.
    pub stations: &'a [Station],
    pub now: OffsetDateTime,
}

impl WeatherContext<'_> {
    /// The fragment for `view` with the current search.
    pub fn fragment_url(&self, view: WeatherView) -> String {
        let mut parameters = vec![("view", view.as_str())];
        if view == WeatherView::List && !self.query.is_empty() {
            parameters.push(("q", self.query));
        }
        with_parameters(self.selection_path, &parameters)
    }

    /// The page URL that shows the same thing, for the address bar.
    pub fn page_url(&self, view: WeatherView) -> String {
        page_url(&self.fragment_url(view))
    }
}

/// `/fragments/weather?…` becomes `/?…`.
pub fn page_url(fragment_url: &str) -> String {
    match fragment_url.split_once('?') {
        Some((_, query)) => format!("/?{query}"),
        None => "/".into(),
    }
}

/// Appends encoded parameters to a path that may already have a query.
pub fn with_parameters(path: &str, parameters: &[(&str, &str)]) -> String {
    let mut url = path.to_string();
    for (name, value) in parameters {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(name);
        url.push('=');
        url.push_str(&encode(value));
    }
    url
}

fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// `hx-sync` for requests the reader starts in the section: they share its
/// queue and replace a refresh in flight.
pub(super) const INTERACTIVE: &str = "closest #weather-table-container:replace";

/// The weather section. It refreshes itself every five minutes, but not
/// while the reader has a station open or is typing a search (`weather.js`).
pub fn weather_section(weather: &[WeatherDisplay], context: &WeatherContext) -> Markup {
    // Searches replace the list only. Read the current input when refreshing
    // the whole section, rather than restoring the query from its initial HTML.
    let refresh = with_parameters(context.selection_path, &[("view", context.view.as_str())]);
    html! {
        // A refresh gives way to a request the reader started (their
        // requests replace it, and it is dropped while one runs).
        section id="weather-table-container"
            hx-get=(refresh)
            hx-include=[(context.view == WeatherView::List).then_some("#weather-search")]
            hx-trigger="every 300s"
            hx-swap="outerHTML"
            hx-sync="this:drop"
            class="box weather" {
            div class="weather-head" {
                h2 class="title is-5" { "Current weather" }
                (view_tabs(context))
            }
            @if let Some(first) = weather.first() {
                (period_note(&first.observation_period, context.now))
            }
            @match context.view {
                WeatherView::Map => (map::weather_map(weather, context)),
                WeatherView::List => {
                    (list::search_form(context))
                    (weather_list(weather, context))
                },
            }
        }
    }
}

fn view_tabs(context: &WeatherContext) -> Markup {
    html! {
        div class="tabs is-toggle is-small weather-views" {
            ul {
                @for (view, label) in [(WeatherView::Map, "Map"), (WeatherView::List, "List")] {
                    li class=[(context.view == view).then_some("is-active")] {
                        a href=(context.page_url(view))
                          hx-get=(context.fragment_url(view))
                          hx-target="#weather-table-container"
                          hx-swap="outerHTML"
                          hx-sync=(INTERACTIVE)
                          hx-push-url=(context.page_url(view))
                          aria-current=[(context.view == view).then_some("true")] {
                            (label)
                        }
                    }
                }
            }
        }
    }
}

fn period_note(period: &ObservationPeriod, now: OffsetDateTime) -> Markup {
    html! {
        p class="weather-period" {
            strong { (period.label()) }
            @if let ObservationPeriod::Selected { start, end } = period {
                " "
                @if let (Some(start), Some(end)) = (when::parse(start), when::parse(end)) {
                    (when::window(start, end))
                }
            }
            " · Δ = observed − forecast"
            @if period.settled(now) == Settled::SoFar {
                span class="weather-provisional" {
                    @if matches!(period, ObservationPeriod::Today) {
                        " · The day isn't over, "
                    } @else {
                        " · This isn't a whole, finished UTC day, "
                    }
                    "so differences stay grey: some hours of reports can't be judged against a whole day's forecast."
                }
            }
        }
    }
}

/// Regions from east to west, as most readers are in the east.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Region {
    Eastern,
    Central,
    Mountain,
    Pacific,
    AlaskaHawaii,
}

impl Region {
    pub(super) fn of(longitude: f64) -> Self {
        match longitude {
            longitude if longitude < -140.0 => Self::AlaskaHawaii,
            longitude if longitude < -115.0 => Self::Pacific,
            longitude if longitude < -100.0 => Self::Mountain,
            longitude if longitude < -85.0 => Self::Central,
            _ => Self::Eastern,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Eastern => "Eastern",
            Self::Central => "Central",
            Self::Mountain => "Mountain",
            Self::Pacific => "Pacific",
            Self::AlaskaHawaii => "Alaska & Hawaii",
        }
    }
}

/// Stations grouped by region, north to south within each.
pub(super) fn by_region<'a>(
    weather: impl IntoIterator<Item = &'a WeatherDisplay>,
) -> Vec<(Region, Vec<&'a WeatherDisplay>)> {
    let mut stations: Vec<_> = weather.into_iter().collect();
    stations.sort_by(|a, b| {
        Region::of(a.longitude)
            .cmp(&Region::of(b.longitude))
            .then(b.latitude.total_cmp(&a.latitude))
            .then(a.station_id.cmp(&b.station_id))
    });
    let mut regions: Vec<(Region, Vec<&WeatherDisplay>)> = vec![];
    for station in stations {
        let region = Region::of(station.longitude);
        match regions.last_mut() {
            Some((last, members)) if *last == region => members.push(station),
            _ => regions.push((region, vec![station])),
        }
    }
    regions
}

pub(super) fn place(weather: &WeatherDisplay) -> String {
    place_name(&weather.station_name, &weather.state)
}

/// "Chicago O'Hare, IL", or whichever part is known.
pub fn place_name(name: &str, state: &str) -> String {
    match (name.is_empty(), state.is_empty()) {
        (false, false) => format!("{name}, {state}"),
        (false, true) => name.to_string(),
        (true, _) => state.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_keep_the_selection_and_encode_the_search() {
        let context = WeatherContext {
            view: WeatherView::List,
            query: "St. Louis & co",
            selection_path: "/fragments/weather?stations=KSTL%2CKORD",
            stations: &[],
            now: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(
            context.fragment_url(WeatherView::List),
            "/fragments/weather?stations=KSTL%2CKORD&view=list&q=St.%20Louis%20%26%20co"
        );
        assert_eq!(
            context.page_url(WeatherView::Map),
            "/?stations=KSTL%2CKORD&view=map"
        );
        assert_eq!(page_url("/fragments/weather"), "/");
        assert_eq!(
            with_parameters("/fragments/weather", &[("view", "list")]),
            "/fragments/weather?view=list"
        );
    }

    #[test]
    fn only_finished_periods_have_settled_differences() {
        let now = OffsetDateTime::parse(
            "2026-09-24T12:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(ObservationPeriod::Today.settled(now), Settled::SoFar);
        let finished = ObservationPeriod::Selected {
            start: "2026-09-23T00:00:00Z".into(),
            end: "2026-09-24T00:00:00Z".into(),
        };
        assert_eq!(finished.settled(now), Settled::Final);
        let open = ObservationPeriod::Selected {
            start: "2026-09-24T00:00:00Z".into(),
            end: "2026-09-25T00:00:00Z".into(),
        };
        assert_eq!(open.settled(now), Settled::SoFar);
        // Part of a day, even one that has ended, is not a whole day.
        let partial = ObservationPeriod::Selected {
            start: "2026-09-23T06:00:00Z".into(),
            end: "2026-09-23T18:00:00Z".into(),
        };
        assert_eq!(partial.settled(now), Settled::SoFar);
        let from_midnight = ObservationPeriod::Selected {
            start: "2026-09-23T00:00:00Z".into(),
            end: "2026-09-23T18:00:00Z".into(),
        };
        assert_eq!(from_midnight.settled(now), Settled::SoFar);
    }
}
