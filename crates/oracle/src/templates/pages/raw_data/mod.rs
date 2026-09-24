use maud::{Markup, html};
use time::{Duration, OffsetDateTime, Time, macros::format_description};

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
        div id="raw-data" class="box raw-data" {
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
                                span id=(format!("{table}-status")) class="tag is-light is-small ml-2" { "Empty" }
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
        assert!(html.contains("data-query=\"-- Daily observations"));
    }

    #[test]
    fn the_example_comparison_is_observed_minus_forecast() {
        let (_, query) = EXAMPLES[2];
        assert!(query.contains("o.temp_high - f.temp_high AS high_difference"));
        assert!(query.contains("o.temp_low - f.temp_low AS low_difference"));
        assert!(!query.contains("f.temp_high - o.temp_high"));
    }
}
