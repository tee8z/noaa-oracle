use maud::{Markup, html};

use crate::templates::{
    fragments::{
        EventStats, WeatherContext, WeatherDisplay, event_stats, oracle_info, weather_section,
    },
    layouts::{CurrentPage, PageConfig, base, page_fragment},
};

/// Dashboard page data
pub struct DashboardData {
    pub pubkey: String,
    pub npub: String,
    pub stats: EventStats,
    pub weather: Vec<WeatherDisplay>,
}

const CONFIG: PageConfig<'static> = PageConfig {
    title: "4cast Truth Oracle - Dashboard",
    current_page: CurrentPage::Dashboard,
};

pub fn dashboard_page(data: &DashboardData, weather: &WeatherContext) -> Markup {
    base(&CONFIG, dashboard_content(data, weather))
}

pub fn dashboard_fragment(data: &DashboardData, weather: &WeatherContext) -> Markup {
    page_fragment(&CONFIG, dashboard_content(data, weather))
}

/// Weather first; event counts, then the oracle's keys, folded away.
fn dashboard_content(data: &DashboardData, weather: &WeatherContext) -> Markup {
    html! {
        (weather_section(&data.weather, weather))
        (event_stats(&data.stats))
        (oracle_info(&data.pubkey, &data.npub))
    }
}
