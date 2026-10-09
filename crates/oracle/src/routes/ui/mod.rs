mod dashboard;
mod docs;
mod event_detail;
mod events;
mod forecast;
mod fragments;
mod htmx;
pub mod local_day;
pub mod policy;
mod raw_data;
mod weather;

/// Installed only by the private metrics/operator listener.
#[derive(Clone, Copy)]
pub struct OperatorView;

pub use dashboard::dashboard_handler;
pub use docs::docs_router;
pub use event_detail::event_detail_handler;
pub use events::events_handler;
pub use forecast::warm_caches;
pub use fragments::{forecast_handler, station_handler, weather_handler};
pub use raw_data::raw_data_handler;
pub use weather::WeatherKey;

/// Background refreshes keep one query working set live at a time. A reader
/// missing the cache keeps the existing parallel path and request deadline.
#[derive(Clone, Copy)]
pub(super) enum QuerySchedule {
    Parallel,
    Serial,
}
impl QuerySchedule {
    async fn join<A: std::future::Future, B: std::future::Future>(
        self,
        a: A,
        b: B,
    ) -> (A::Output, B::Output) {
        match self {
            Self::Parallel => tokio::join!(a, b),
            Self::Serial => (a.await, b.await),
        }
    }
    async fn try_join<A, B, T, U, E>(self, a: A, b: B) -> Result<(T, U), E>
    where
        A: std::future::Future<Output = Result<T, E>>,
        B: std::future::Future<Output = Result<U, E>>,
    {
        match self {
            Self::Parallel => tokio::try_join!(a, b),
            Self::Serial => Ok((a.await?, b.await?)),
        }
    }
}
