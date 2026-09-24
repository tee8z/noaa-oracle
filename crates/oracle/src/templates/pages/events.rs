use maud::Markup;
use time::OffsetDateTime;

use crate::templates::{
    fragments::events::{EventFilters, EventView, events_section},
    layouts::{CurrentPage, PageConfig, base, page_fragment},
};

const CONFIG: PageConfig<'static> = PageConfig {
    title: "4cast Truth Oracle - Events",
    current_page: CurrentPage::Events,
};

pub fn events_page(events: &[EventView], filters: EventFilters, now: OffsetDateTime) -> Markup {
    base(&CONFIG, events_section(events, filters, now))
}

pub fn events_fragment(events: &[EventView], filters: EventFilters, now: OffsetDateTime) -> Markup {
    page_fragment(&CONFIG, events_section(events, filters, now))
}
