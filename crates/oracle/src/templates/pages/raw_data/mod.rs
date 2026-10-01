use maud::{Markup, html};
use time::{Duration, OffsetDateTime, Time, macros::format_description};

use crate::templates::assets;
use crate::templates::layouts::{CurrentPage, PageConfig, base};

const CONFIG: PageConfig<'static> = PageConfig {
    title: "4cast Truth Oracle - Raw Data",
    current_page: CurrentPage::RawData,
};

/// Example queries, run by DuckDB in the browser against the loaded files.
const EXAMPLES: [(&str, &str); 4] = [
    (
        "Daily observations",
        include_str!("queries/daily_observations.sql"),
    ),
    ("Daily forecast", include_str!("queries/daily_forecast.sql")),
    (
        "Forecast vs observed",
        include_str!("queries/forecast_vs_observed.sql"),
    ),
    ("Station list", include_str!("queries/stations.sql")),
];

pub fn raw_data_page(now: OffsetDateTime) -> Markup {
    base(&CONFIG, raw_data_content(now))
}

/// Yesterday, midnight to midnight UTC, as `datetime-local` values.
fn yesterday(now: OffsetDateTime) -> (String, String) {
    let today = now.replace_time(Time::MIDNIGHT);
    let format = format_description!("[year]-[month]-[day]T[hour]:[minute]");
    (
        (today - Duration::days(1))
            .format(format)
            .unwrap_or_default(),
        today.format(format).unwrap_or_default(),
    )
}

/// The parquet analyzer. The page's script loads DuckDB-WASM and queries
/// the files in the browser; the server only lists and serves them.
pub fn raw_data_content(now: OffsetDateTime) -> Markup {
    let (start, end) = yesterday(now);
    html! {
        div id="raw-data" class="box raw-data" data-duckdb-module=(assets::DUCKDB_JS.url) {
            h2 class="title is-5" { "NOAA forecast and observation data" }
            p class="muted is-size-7 mb-3" {
                "Load the parquet files for a window, then query them with DuckDB in your browser. "
                "Forecast files are several MB each; a full day is over 100 MB."
            }

            div class="raw-controls" {
                div class="field" {
                    label class="label is-small" for="start" { "Start (UTC)" }
                    input id="start" class="input is-small" type="datetime-local" value=(start);
                }
                div class="field" {
                    label class="label is-small" for="end" { "End (UTC)" }
                    input id="end" class="input is-small" type="datetime-local" value=(end);
                }
                fieldset class="field raw-kinds" {
                    legend class="label is-small" { "Files" }
                    label class="checkbox" { input id="observations" type="checkbox" checked; " Observations" }
                    label class="checkbox" { input id="forecasts" type="checkbox" checked; " Forecasts" }
                }
                button id="submit" class="button is-link is-small" data-needs-db disabled {
                    span class="icon" { (download_icon()) }
                    span { "Load files" }
                }
            }
            p id="raw-data-status" class="raw-status" role="status" { "Loading DuckDB…" }

            div class="columns is-multiline mb-4" {
                @for (table, title) in [("forecasts", "Forecasts schema"), ("observations", "Observations schema")] {
                    div class="column is-full-mobile is-half-desktop" {
                        div class="schema-box" {
                            div class="schema-header" {
                                span class="schema-title" { (title) }
                                span id=(format!("{table}-status")) class="tag is-small ml-2" { "Empty" }
                            }
                            div id=(format!("{table}-loading")) class="schema-loading" style="display: none;" {
                                span class="loader mr-2" {}
                                "Loading " (table) "…"
                            }
                            textarea id=(format!("{table}-schema")) class="schema-content is-size-7" readonly
                                placeholder="The schema appears here after the files load." {}
                        }
                    }
                }
            }

            div class="field" {
                p class="label is-small" { "Example queries" }
                p class="help" {
                    "Examples use UTC days and Fahrenheit temperatures. Precipitation columns show "
                    "reported interval maxima, not daily totals. Forecast intervals can cross midnight."
                }
                div class="buttons are-small" {
                    @for (label, query) in EXAMPLES {
                        button type="button" class="button" data-query=(query) { (label) }
                    }
                }
            }

            div class="field" {
                label class="label is-small" for="customQuery" { "Query" }
                textarea id="customQuery" class="textarea is-family-code is-size-7" rows="5" {
                    "SELECT * FROM observations ORDER BY station_id, generated_at DESC LIMIT 200"
                }
                p class="help" {
                    "DuckDB SQL. Tables: observations, forecasts. "
                    a href="https://duckdb.org/docs/sql/introduction" { "Query documentation" }
                }
            }

            div class="buttons are-small" {
                button id="runQuery" class="button is-link" data-needs-db disabled {
                    span class="icon" { (play_icon()) }
                    span { "Run query" }
                }
                button id="downloadCsv" class="button is-link" disabled {
                    span class="icon" { (download_icon()) }
                    span { "Download CSV" }
                }
                button id="clearQuery" class="button" { "Clear" }
            }

            div class="query-result-wrapper" id="queryResult-container" {}
        }
    }
}

fn download_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" {}
            polyline points="7 10 12 15 17 10" {}
            line x1="12" y1="15" x2="12" y2="3" {}
        }
    }
}

fn play_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            polygon points="5 3 19 12 5 21 5 3" {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn defaults_to_yesterday_utc_with_both_kinds_ticked() {
        let html = raw_data_content(datetime!(2026-09-24 00:30 UTC)).into_string();
        assert!(html.contains(r#"value="2026-09-23T00:00""#), "{html}");
        assert!(html.contains(r#"value="2026-09-24T00:00""#));
        assert_eq!(html.matches("type=\"checkbox\" checked").count(), 2);
        assert!(html.contains("data-query=\"-- Exploratory daily report"));
    }

    #[test]
    fn examples_normalize_units_and_do_not_sum_overlapping_precipitation() {
        let connection = duckdb::Connection::open_in_memory().unwrap();
        connection.execute_batch(r#"
            CREATE TABLE observations AS
            SELECT station_id, generated_at, temperature_value::DOUBLE AS temperature_value,
                'celsius' AS temperature_unit_code, 18.0::DOUBLE AS dewpoint_value,
                8::BIGINT AS wind_speed, 90::BIGINT AS wind_direction,
                precip_in::DOUBLE AS precip_in, metar_type, filename
            FROM (VALUES
                ('KORD', '2026-01-17T12:00:00Z', 30.0, 0.8, 'METAR', 'observations_2026-01-17T12:10:00Z.parquet'),
                ('KORD', '2026-01-17T12:00:00Z', 20.0, 0.4, 'METAR', 'observations_2026-01-17T12:20:00Z.parquet'),
                ('KORD', '2026-01-17T12:30:00Z', 20.0, 0.7, 'SPECI', 'observations_2026-01-17T12:40:00Z.parquet')
            ) AS reports(station_id, generated_at, temperature_value, precip_in, metar_type, filename);
            CREATE TABLE forecasts AS
            SELECT 'KORD' AS station_id, '2026-01-17T00:00:00Z' AS begin_time,
                '2026-01-18T00:00:00Z' AS end_time, '2026-01-16T12:00:00Z' AS generated_at,
                68.0::DOUBLE AS min_temp, 68.0::DOUBLE AS max_temp,
                'fahrenheit' AS temperature_unit_code,
                'forecasts_2026-01-16T12:30:00Z.parquet' AS filename;
        "#).unwrap();
        let mut observations = connection.prepare(EXAMPLES[0].1).unwrap();
        let (reports, high, precip): (i64, f64, f64) = observations
            .query_row([], |row| Ok((row.get(1)?, row.get(4)?, row.get(7)?)))
            .unwrap();
        assert_eq!(reports, 2, "the corrected snapshot replaces the old one");
        assert_eq!(high, 68.0);
        assert_eq!(
            precip, 0.4,
            "overlapping special reports cannot inflate a total"
        );
        let mut comparison = connection.prepare(EXAMPLES[2].1).unwrap();
        let differences: (f64, f64) = comparison
            .query_row([], |row| Ok((row.get(6)?, row.get(7)?)))
            .unwrap();
        assert_eq!(
            differences,
            (0.0, 0.0),
            "20 Celsius and 68 Fahrenheit agree"
        );
    }
}
