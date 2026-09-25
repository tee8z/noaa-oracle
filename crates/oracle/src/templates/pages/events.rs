use maud::Markup;

use crate::templates::{
    fragments::events::{EventsPage, events_section},
    layouts::{CurrentPage, PageConfig, base, page_fragment},
};

const CONFIG: PageConfig<'static> = PageConfig {
    title: "4cast Truth Oracle - Events",
    current_page: CurrentPage::Events,
};

pub fn events_page(page: &EventsPage) -> Markup {
    base(&CONFIG, events_section(page))
}

pub fn events_fragment(page: &EventsPage) -> Markup {
    page_fragment(&CONFIG, events_section(page))
}
