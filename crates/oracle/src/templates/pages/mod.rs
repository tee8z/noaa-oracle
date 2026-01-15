pub mod dashboard;
pub mod event_detail;
pub mod events;
pub mod raw_data;

pub use dashboard::dashboard_page;
pub use event_detail::{event_detail_content, event_detail_page};
pub use events::events_page;
pub use raw_data::raw_data_page;
