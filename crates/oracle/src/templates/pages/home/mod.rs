use maud::{html, Markup};

use crate::templates::layouts::{base, PageConfig};

pub fn home_page(api_base: &str) -> Markup {
    let config = PageConfig {
        title: "NOAA Data Visualizer",
        api_base,
    };

    base(&config, content())
}

fn content() -> Markup {
    html! {
        section class="section" {
            div class="container" {
                // Header
                nav class="level" {
                    div class="level-left" {
                        h1 class="title level-item" {
                            "NOAA Forecast and Observation Data Analyzer"
                        }
                    }
                    p class="level-item" {
                        a href="/docs" class="button is-link is-light is-small" {
                            "API Docs"
                        }
                    }
                    div class="level-right" {
                        p class="level-item" {
                            a href="https://github.com/tee8z/noaa-oracle" target="_blank" 
                              style="font-size: 0.7em; margin-left: 10px; color: #333;" {
                                (github_icon())
                            }
                        }
                    }
                }

                // Date filters
                div class="field is-grouped" {
                    div class="control" {
                        div class="field is-horizontal" {
                            div class="field-label is-normal" {
                                label class="label" { "Start" }
                            }
                            div class="field-body" {
                                div class="field" {
                                    p class="control" {
                                        input id="start" type="datetime" autocomplete="on" 
                                              min="2024-01-01T00:00:00Z";
                                    }
                                }
                            }
                        }
                    }
                    div class="control" {
                        div class="field is-horizontal" {
                            div class="field-label is-normal" {
                                label class="label" { "End" }
                            }
                            div class="field-body" {
                                div class="field" {
                                    p class="control" {
                                        input id="end" type="datetime" autocomplete="on" 
                                              max="2035-01-01T00:00:00Z";
                                    }
                                }
                            }
                        }
                    }
                    div class="control" {
                        label class="checkbox" {
                            input id="observations" type="checkbox";
                            " Observations"
                        }
                    }
                    div class="control" {
                        label class="checkbox" {
                            input id="forecasts" type="checkbox";
                            " Forecasts"
                        }
                    }
                }

                div class="control" {
                    button id="submit" class="button is-primary" {
                        span { "Download Files" }
                    }
                }

                // Schema display
                div {
                    h4 { "Schemas of the files" }
                    div class="field" {
                        pre id="forecasts-schema" {}
                    }
                    div class="field" {
                        pre id="observations-schema" {}
                    }
                }

                // Custom query
                div class="field" {
                    label class="label" { "Custom Query" }
                    div class="control" {
                        textarea id="customQuery" class="textarea" 
                                 placeholder="place custom query here" {}
                    }
                    h4 {
                        "Learn how to make queries from: "
                        a href="https://duckdb.org/docs/sql/introduction" target="_blank" {
                            span { "Query Docs" }
                        }
                    }
                }

                div class="field is-grouped" {
                    div class="control" {
                        button id="runQuery" class="button is-info is-light" {
                            span { "Run" }
                        }
                    }
                    div class="control" {
                        button id="clearQuery" class="button is-danger is-light" {
                            span { "Clear" }
                        }
                    }
                }

                div class="table-container" id="queryResult-container" {}
            }
        }
    }
}

fn github_icon() -> Markup {
    html! {
        svg height="24" width="24" viewBox="0 0 16 16" version="1.1" 
            aria-hidden="true" style="fill: currentColor; vertical-align: middle;" {
            path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" {}
        }
    }
}
