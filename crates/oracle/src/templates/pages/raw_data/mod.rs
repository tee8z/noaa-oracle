use maud::{html, Markup};

use crate::templates::layouts::{base, CurrentPage, PageConfig};

/// Raw data page - wrapper for the existing DuckDB-WASM parquet analyzer
pub fn raw_data_page(api_base: &str) -> Markup {
    let config = PageConfig {
        title: "4cast Truth Oracle - Raw Data",
        api_base,
        current_page: CurrentPage::RawData,
    };

    base(&config, raw_data_content())
}

/// Raw data content - the parquet file analyzer
pub fn raw_data_content() -> Markup {
    html! {
        div class="box" {
            h2 class="title is-5 mb-4" { "NOAA Forecast and Observation Data Analyzer" }

            // Date filters
            div class="field is-grouped is-grouped-multiline" {
                div class="control" {
                    div class="field" {
                        label class="label is-small" { "Start" }
                        input id="start" class="input" type="datetime-local" autocomplete="on";
                    }
                }
                div class="control" {
                    div class="field" {
                        label class="label is-small" { "End" }
                        input id="end" class="input" type="datetime-local" autocomplete="on";
                    }
                }
                div class="control" {
                    div class="field" {
                        label class="label is-small" { "\u{00A0}" } // Non-breaking space for alignment
                        div class="field is-grouped" {
                            label class="checkbox mr-4" {
                                input id="observations" type="checkbox";
                                " Observations"
                            }
                            label class="checkbox" {
                                input id="forecasts" type="checkbox";
                                " Forecasts"
                            }
                        }
                    }
                }
            }

            div class="control mb-4" {
                button id="submit" class="button is-primary" {
                    span class="icon" { (download_icon()) }
                    span { "Download Files" }
                }
            }

            // Schema display - resizable textareas with loading states
            div class="columns is-multiline mb-4" {
                div class="column is-full-mobile is-half-desktop" {
                    div class="schema-box" {
                        div class="schema-header" {
                            span class="schema-title" {
                                " Forecasts Schema"
                            }
                            span id="forecasts-status" class="tag is-light is-small ml-2" { "Empty" }
                        }
                        div id="forecasts-loading" class="schema-loading" style="display: none;" {
                            span class="loader mr-2" {}
                            "Loading forecasts..."
                        }
                        textarea id="forecasts-schema" class="schema-content is-size-7" readonly placeholder="Schema will appear here after downloading files..." {}
                    }
                }
                div class="column is-full-mobile is-half-desktop" {
                    div class="schema-box" {
                        div class="schema-header" {
                            span class="schema-title" {
                                " Observations Schema"
                            }
                            span id="observations-status" class="tag is-light is-small ml-2" { "Empty" }
                        }
                        div id="observations-loading" class="schema-loading" style="display: none;" {
                            span class="loader mr-2" {}
                            "Loading observations..."
                        }
                        textarea id="observations-schema" class="schema-content is-size-7" readonly placeholder="Schema will appear here after downloading files..." {}
                    }
                }
            }

            // Custom query
            div class="field" {
                label class="label" { "Custom Query" }
                div class="control" {
                    textarea id="customQuery" class="textarea" rows="4"
                             placeholder="SELECT * FROM observations ORDER BY station_id, generated_at DESC LIMIT 200" {}
                }
                p class="help" {
                    "Use DuckDB SQL syntax. "
                    a href="https://duckdb.org/docs/sql/introduction" target="_blank" {
                        "Query Documentation"
                    }
                }
            }

            div class="field is-grouped mb-4" {
                div class="control" {
                    button id="runQuery" class="button is-info" {
                        span class="icon" { (play_icon()) }
                        span { "Run Query" }
                    }
                }
                div class="control" {
                    button id="clearQuery" class="button is-light" {
                        span { "Clear" }
                    }
                }
                div class="control" {
                    button id="downloadCsv" class="button is-success" disabled {
                        span class="icon" { (download_icon()) }
                        span { "Download CSV" }
                    }
                }
            }

            // Query results
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
