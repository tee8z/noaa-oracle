use maud::{html, Markup};

use crate::templates::{
    fragments::{event_stats, oracle_info, weather_table, EventStats, WeatherDisplay},
    layouts::{base, CurrentPage, PageConfig},
};

/// Dashboard page data
pub struct DashboardData {
    pub pubkey: String,
    pub npub: String,
    pub stats: EventStats,
    pub weather: Vec<WeatherDisplay>,
    pub all_stations: Vec<(String, String)>,
}

/// Dashboard page - shows oracle info, event stats, and weather data
pub fn dashboard_page(api_base: &str, data: &DashboardData) -> Markup {
    let config = PageConfig {
        title: "4cast Truth Oracle - Dashboard",
        api_base,
        current_page: CurrentPage::Dashboard,
    };

    base(&config, dashboard_content(data))
}

/// Dashboard content - can be used for full page or HTMX partial
pub fn dashboard_content(data: &DashboardData) -> Markup {
    html! {
        // Oracle Identity
        (oracle_info(&data.pubkey, &data.npub))

        // Event Statistics
        div class="mt-4" {
            (event_stats(&data.stats))
        }

        // Weather Data
        div class="mt-4" {
            (weather_table(&data.weather, &data.all_stations))
        }
    }
}
