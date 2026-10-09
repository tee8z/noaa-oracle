//! Work whose cost grows with every station or with weeks of files:
//! eligible station lists, discovery answers, window assessments,
//! processing passes, file preparation and cache warming. Run side by
//! side, a few such jobs took the oracle to its memory ceiling, where it
//! signs attestations, so they take turns from one budget, [`HeavyWork`]:
//!
//! - A request that has to build such a value builds it on one of
//!   [`HEAVY_TURNS`] turns. The build waits at most [`HEAVY_WAIT`], behind
//!   at most [`QUEUED_HEAVY_WORK`] others, and the request gets 503 with
//!   `Retry-After` past that. Values are kept per question ([`Kept`]), so a
//!   repeated question costs nothing until new data arrives.
//! - Builds run in tasks of their own. A reader who stops waiting after
//!   [`HEAVY_REQUEST_TIMEOUT`] gets 503, and the value is kept for the next
//!   one.
//! - Processing passes, file preparation and the reading of eligibility
//!   reports take every turn, so nothing heavy runs beside them. They wait
//!   for the work already running. Delays are logged every [`PASS_WAIT`];
//!   a pass never bypasses this memory budget.
//! - Cache warming takes one turn per value and waits as long as it must,
//!   so a processing pass waits for at most the values being built.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
    AppError,
    cache::{Cache, Cached},
    weather_data,
};

/// Heavy work running at once.
pub const HEAVY_TURNS: u32 = 2;
/// Heavy requests that wait for a turn, beyond those running. Past this the
/// oracle turns them away at once.
const QUEUED_HEAVY_WORK: usize = 4;
/// Longest a heavy request waits for a turn.
const HEAVY_WAIT: Duration = Duration::from_secs(10);
/// Longest a request waits for heavy work to finish. The work goes on and
/// its value is kept.
pub const HEAVY_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Interval between warnings while a pass waits for existing heavy work.
pub const PASS_WAIT: Duration = Duration::from_secs(120);
/// Seconds a turned away client is asked to wait before it retries.
pub const RETRY_AFTER_SECONDS: u64 = 5;
/// Fewest seconds between memory releases in loops that free as they go.
const RELEASE_SPACING: Duration = Duration::from_secs(10);

/// Why heavy work was turned away.
pub const NO_TURN: &str = "the oracle is busy with other heavy work; try again shortly";
pub const STILL_WORKING: &str = "the answer is still being prepared; try again shortly";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Turns for work of one kind: a fixed number at once, and up to
/// `max_waiting` more waiting for one.
pub(crate) struct Admission {
    turns: Arc<Semaphore>,
    pub(crate) waiting: AtomicUsize,
    max_waiting: usize,
}

/// One waiting request, counted until it gets a turn or gives up.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Admission {
    pub(crate) fn new(turns: usize, max_waiting: usize) -> Self {
        Self {
            turns: Arc::new(Semaphore::new(turns)),
            waiting: AtomicUsize::new(0),
            max_waiting,
        }
    }

    /// A turn, now or after a wait, or `None` when too many requests wait
    /// already or none came within `wait`.
    pub(crate) async fn turn(&self, wait: Duration) -> Option<OwnedSemaphorePermit> {
        self.turns_at_once(1, wait).await
    }

    /// `count` turns together, as [`Self::turn`] takes one.
    async fn turns_at_once(&self, count: u32, wait: Duration) -> Option<OwnedSemaphorePermit> {
        if let Ok(turns) = self.turns.clone().try_acquire_many_owned(count) {
            return Some(turns);
        }
        if self.waiting.fetch_add(1, Ordering::AcqRel) >= self.max_waiting {
            self.waiting.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let _waiting = Waiting(&self.waiting);
        tokio::time::timeout(wait, self.turns.clone().acquire_many_owned(count))
            .await
            .ok()?
            .ok()
    }
}

/// How many turns a piece of heavy work takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turns {
    /// One, beside other heavy work.
    One,
    /// Every turn: work that takes too much memory to run beside any other,
    /// such as judging weeks of reports.
    Every,
}

/// The budget heavy work takes turns from (see the module documentation).
/// Turns are handed out in the order they were asked for, so a pass that
/// asks for every turn is not overtaken by work that asks after it.
pub struct HeavyWork {
    admission: Admission,
}

impl Default for HeavyWork {
    fn default() -> Self {
        Self::new()
    }
}

impl HeavyWork {
    pub fn new() -> Self {
        Self {
            admission: Admission::new(HEAVY_TURNS as usize, QUEUED_HEAVY_WORK),
        }
    }

    /// A turn for heavy work a request asked for: at once, or within
    /// [`HEAVY_WAIT`] behind at most [`QUEUED_HEAVY_WORK`] others.
    pub async fn turn(&self) -> Option<OwnedSemaphorePermit> {
        self.admission.turn(HEAVY_WAIT).await
    }

    /// Every turn, as [`Self::turn`] takes one: for requested work that
    /// takes too much memory to run beside any other, such as judging a
    /// month of reports.
    pub async fn every_turn_for_request(&self) -> Option<OwnedSemaphorePermit> {
        self.admission.turns_at_once(HEAVY_TURNS, HEAVY_WAIT).await
    }

    /// [`Self::turn`] or [`Self::every_turn_for_request`].
    pub async fn turns_for_request(&self, turns: Turns) -> Option<OwnedSemaphorePermit> {
        match turns {
            Turns::One => self.turn().await,
            Turns::Every => self.every_turn_for_request().await,
        }
    }

    /// A turn for background work that can wait, whenever one is free.
    pub async fn patient_turn(&self) -> Option<OwnedSemaphorePermit> {
        self.admission.turns.clone().acquire_owned().await.ok()
    }

    /// Every turn, for a pass that should run alone, once the work holding
    /// turns finishes; `None` if that takes longer than `wait`.
    #[cfg(test)]
    pub async fn every_turn(&self, wait: Duration) -> Option<OwnedSemaphorePermit> {
        tokio::time::timeout(
            wait,
            self.admission.turns.clone().acquire_many_owned(HEAVY_TURNS),
        )
        .await
        .ok()?
        .ok()
    }

    /// Wait for exclusive background admission without losing FIFO position.
    pub async fn patient_every_turn(&self) -> Option<OwnedSemaphorePermit> {
        self.admission
            .turns
            .clone()
            .acquire_many_owned(HEAVY_TURNS)
            .await
            .ok()
    }

    /// Turns free now.
    pub fn free_turns(&self) -> usize {
        self.admission.turns.available_permits()
    }

    /// Requests waiting for a turn now.
    pub fn waiting(&self) -> usize {
        self.admission.waiting.load(Ordering::Acquire)
    }
}

/// What every reader waiting for one build gets.
type Build<V> = Shared<BoxFuture<'static, Result<V, Arc<AppError>>>>;

/// How to build a value, should it need building.
struct Started<F> {
    heavy: Arc<HeavyWork>,
    turns: Turns,
    /// The data generation the value is built from, read before building.
    generation: u64,
    build: F,
}

/// Values that heavy work builds, kept per question until the data they
/// were built from changes or they age (as a [`Cache`]). A value is built
/// once however many readers ask for it at the same time, on a heavy turn,
/// in a task of its own. A stale value is served while one rebuild runs,
/// but not once it is older than `max_stale`.
pub(crate) struct Kept<K, V> {
    values: Mutex<Cache<K, (Instant, V)>>,
    builds: Mutex<HashMap<K, Build<V>>>,
    max_stale: Duration,
    /// Estimated retained bytes of a key and its value.
    size: fn(&K, &V) -> usize,
}

/// Ends a build: forgets it, and lets the next reader rebuild a stale value
/// if this build failed or panicked.
struct Finished<'a, K: Clone + Eq + Hash, V: Clone> {
    kept: &'a Kept<K, V>,
    key: &'a K,
}

impl<K: Clone + Eq + Hash, V: Clone> Drop for Finished<'_, K, V> {
    fn drop(&mut self) {
        lock(&self.kept.values).refresh_failed(self.key);
        lock(&self.kept.builds).remove(self.key);
    }
}

impl<K, V> Kept<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub(crate) fn new(
        capacity: usize,
        max_age: Duration,
        max_bytes: usize,
        max_stale: Duration,
        size: fn(&K, &V) -> usize,
    ) -> Self {
        Self {
            values: Mutex::new(Cache::with_byte_limit(capacity, max_age, max_bytes)),
            builds: Mutex::new(HashMap::new()),
            max_stale,
            size,
        }
    }

    /// The value for `key`, fresh for data `generation`: kept, or built
    /// now by `build` on `turns` heavy turns and kept. Waits at most
    /// [`HEAVY_REQUEST_TIMEOUT`] for a build; the build goes on after that.
    pub(crate) async fn get<F, Fut>(
        self: &Arc<Self>,
        heavy: &Arc<HeavyWork>,
        turns: Turns,
        key: K,
        generation: u64,
        build: F,
    ) -> Result<V, AppError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, AppError>> + Send + 'static,
    {
        let build = Started {
            heavy: heavy.clone(),
            turns,
            generation,
            build,
        };
        self.get_within(key, build, HEAVY_REQUEST_TIMEOUT).await
    }

    /// [`Self::get`], waiting at most `timeout` for a build.
    async fn get_within<F, Fut>(
        self: &Arc<Self>,
        key: K,
        build: Started<F>,
        timeout: Duration,
    ) -> Result<V, AppError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, AppError>> + Send + 'static,
    {
        let cached = lock(&self.values).get(&key, build.generation);
        match cached {
            Cached::Fresh((_, value)) => return Ok(value),
            Cached::Stale {
                value: (built, value),
                refresh,
            } if built.elapsed() < self.max_stale => {
                if refresh {
                    // Readers keep the stale value; the build fills the cache.
                    drop(self.start(key, build));
                }
                return Ok(value);
            }
            Cached::Stale { .. } | Cached::Missing => {}
        }
        let running = self.start(key, build);
        match tokio::time::timeout(timeout, running).await {
            Ok(built) => built.map_err(AppError::Shared),
            Err(_) => Err(AppError::Busy(STILL_WORKING)),
        }
    }

    /// The build running for `key`, or a new one.
    fn start<F, Fut>(self: &Arc<Self>, key: K, build: Started<F>) -> Build<V>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, AppError>> + Send + 'static,
    {
        let mut builds = lock(&self.builds);
        if let Some(running) = builds.get(&key) {
            return running.clone();
        }
        let Started {
            heavy,
            turns,
            generation,
            build,
        } = build;
        let kept = self.clone();
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            let _finished = Finished {
                kept: &kept,
                key: &task_key,
            };
            let built = match heavy.turns_for_request(turns).await {
                Some(_turn) => {
                    let built = build().await;
                    release_freed_memory();
                    built
                }
                None => Err(AppError::Busy(NO_TURN)),
            };
            if let Ok(value) = &built {
                let bytes = (kept.size)(&task_key, value);
                lock(&kept.values).insert_sized(
                    task_key.clone(),
                    (Instant::now(), value.clone()),
                    generation,
                    bytes,
                );
            }
            built.map_err(Arc::new)
        });
        let running: Build<V> = async move {
            match task.await {
                Ok(built) => built,
                Err(error) => Err(Arc::new(AppError::from(weather_data::Error::Task(error)))),
            }
        }
        .boxed()
        .shared();
        builds.insert(key, running.clone());
        running
    }

    pub(crate) fn len(&self) -> usize {
        lock(&self.values).len()
    }

    pub(crate) fn bytes(&self) -> usize {
        lock(&self.values).bytes()
    }
}

/// Returns freed heap memory to the system. glibc keeps what large queries
/// and caches freed in its arenas, so without this the process stays at its
/// peak size.
pub fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: malloc_trim only releases free memory and may be called
        // from any thread at any time.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// As [`release_freed_memory`], at most once every [`RELEASE_SPACING`]:
/// for loops that free memory as they go, such as cache warming.
pub fn release_freed_memory_now_and_then() {
    static RELEASED: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut released = lock(&RELEASED);
        if released.is_some_and(|at| at.elapsed() < RELEASE_SPACING) {
            return;
        }
        *released = Some(Instant::now());
    }
    release_freed_memory();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    const KEPT_FOR: Duration = Duration::from_secs(60);

    fn kept() -> Arc<Kept<&'static str, u32>> {
        Arc::new(Kept::new(8, KEPT_FOR, 1024, KEPT_FOR, |_, _| 8))
    }

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("test timed out")
    }

    #[tokio::test]
    async fn exclusive_background_work_waits_and_blocks_later_warmers() {
        let heavy = HeavyWork::new();
        let running = heavy.turn().await.unwrap();
        let exclusive = heavy.patient_every_turn();
        tokio::pin!(exclusive);
        // Poll but retain the same future after a diagnostic interval.
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut exclusive)
                .await
                .is_err()
        );
        assert!(heavy.admission.turns.clone().try_acquire_owned().is_err());
        drop(running);
        let all = bounded(&mut exclusive).await.unwrap();
        assert_eq!(heavy.free_turns(), 0);
        drop(all);
        assert_eq!(heavy.free_turns(), HEAVY_TURNS as usize);
    }

    #[tokio::test]
    async fn a_pass_takes_every_turn_and_requests_wait_behind_it() {
        let heavy = Arc::new(HeavyWork::new());
        let request = heavy.turn().await.unwrap();
        // The pass waits for the request already running ...
        let pass = tokio::spawn({
            let heavy = heavy.clone();
            async move { heavy.every_turn(Duration::from_secs(4)).await }
        });
        bounded(async {
            while heavy.free_turns() > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        // ... and work asked for after it does not overtake it.
        let later = tokio::spawn({
            let heavy = heavy.clone();
            async move { heavy.turn().await.is_some() }
        });
        drop(request);
        let pass = bounded(pass).await.unwrap().expect("every turn");
        assert_eq!(heavy.free_turns(), 0);
        assert!(!later.is_finished());
        drop(pass);
        assert!(bounded(later).await.unwrap());
        assert_eq!(heavy.free_turns(), HEAVY_TURNS as usize);
    }

    #[tokio::test]
    async fn a_pass_runs_anyway_when_heavy_work_outlasts_its_wait() {
        let heavy = HeavyWork::new();
        let _running = heavy.turn().await.unwrap();
        assert!(heavy.every_turn(Duration::from_millis(20)).await.is_none());
        // Giving up returned the turns it had gathered.
        assert_eq!(heavy.free_turns(), HEAVY_TURNS as usize - 1);
    }

    #[tokio::test]
    async fn requests_beyond_the_queue_are_turned_away_at_once() {
        let heavy = Arc::new(HeavyWork::new());
        let _all = heavy.every_turn(Duration::ZERO).await.unwrap();
        let waiting: Vec<_> = (0..QUEUED_HEAVY_WORK)
            .map(|_| {
                let heavy = heavy.clone();
                tokio::spawn(async move { heavy.turn().await.is_some() })
            })
            .collect();
        bounded(async {
            while heavy.waiting() < QUEUED_HEAVY_WORK {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let started = Instant::now();
        assert!(heavy.turn().await.is_none());
        assert!(heavy.every_turn_for_request().await.is_none());
        assert!(started.elapsed() < Duration::from_secs(1), "not at once");
        for task in waiting {
            task.abort();
        }
    }

    #[tokio::test]
    async fn readers_asking_together_share_one_build_and_the_value_is_kept() {
        let heavy = Arc::new(HeavyWork::new());
        let kept = kept();
        let builds = Arc::new(AtomicU32::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let (heavy, kept, builds, release) =
                    (heavy.clone(), kept.clone(), builds.clone(), release.clone());
                tokio::spawn(async move {
                    kept.get(&heavy, Turns::One, "KORD", 0, move || async move {
                        builds.fetch_add(1, Ordering::SeqCst);
                        release.notified().await;
                        Ok(7)
                    })
                    .await
                })
            })
            .collect();
        bounded(async {
            while builds.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        release.notify_one();
        for reader in readers {
            assert_eq!(bounded(reader).await.unwrap().unwrap(), 7);
        }
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(heavy.free_turns(), HEAVY_TURNS as usize);
        let value = kept
            .get(&heavy, Turns::One, "KORD", 0, || async { Ok(99) })
            .await
            .unwrap();
        assert_eq!(value, 7, "kept values are not rebuilt");
        assert_eq!(kept.len(), 1);
    }

    #[tokio::test]
    async fn new_data_serves_the_kept_value_while_one_rebuild_runs() {
        let heavy = Arc::new(HeavyWork::new());
        let kept = kept();
        kept.get(&heavy, Turns::One, "KORD", 0, || async { Ok(1) })
            .await
            .unwrap();
        let value = kept
            .get(&heavy, Turns::One, "KORD", 1, || async { Ok(2) })
            .await
            .unwrap();
        assert_eq!(value, 1, "stale, while it is rebuilt");
        bounded(async {
            loop {
                // Readers meanwhile do not start rebuilds of their own.
                let value = kept
                    .get(&heavy, Turns::One, "KORD", 1, || async { Ok(99) })
                    .await
                    .unwrap();
                assert_ne!(value, 99, "one rebuild at a time");
                if value == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn failures_are_shared_but_not_kept() {
        let heavy = Arc::new(HeavyWork::new());
        let kept = kept();
        let failed = kept
            .get(&heavy, Turns::One, "KORD", 0, || async {
                Err(AppError::InvalidRequest("no".into()))
            })
            .await;
        assert!(matches!(failed, Err(AppError::Shared(_))));
        assert_eq!(kept.len(), 0);
        let value = kept
            .get(&heavy, Turns::One, "KORD", 0, || async { Ok(3) })
            .await
            .unwrap();
        assert_eq!(value, 3);
    }

    #[tokio::test]
    async fn without_a_turn_a_build_is_turned_away_and_retried_later() {
        let heavy = Arc::new(HeavyWork::new());
        let kept = kept();
        let all = heavy.every_turn(Duration::ZERO).await.unwrap();
        // Fill the queue so the build cannot wait for a turn.
        let waiting: Vec<_> = (0..QUEUED_HEAVY_WORK)
            .map(|_| {
                let heavy = heavy.clone();
                tokio::spawn(async move { heavy.turn().await.is_some() })
            })
            .collect();
        bounded(async {
            while heavy.waiting() < QUEUED_HEAVY_WORK {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let busy = kept
            .get(&heavy, Turns::One, "KORD", 0, || async { Ok(1) })
            .await
            .unwrap_err();
        let busy = match busy {
            AppError::Shared(busy) => busy,
            other => panic!("{other:?}"),
        };
        assert!(matches!(busy.as_ref(), AppError::Busy(NO_TURN)));
        for task in waiting {
            task.abort();
        }
        drop(all);
        bounded(async {
            while heavy.waiting() > 0 || heavy.free_turns() < HEAVY_TURNS as usize {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let value = kept
            .get(&heavy, Turns::One, "KORD", 0, || async { Ok(4) })
            .await
            .unwrap();
        assert_eq!(value, 4);
    }

    #[tokio::test]
    async fn a_reader_stops_waiting_but_the_build_finishes_and_is_kept() {
        let heavy = Arc::new(HeavyWork::new());
        let kept = kept();
        let release = Arc::new(tokio::sync::Notify::new());
        let held = release.clone();
        let build = Started {
            heavy: heavy.clone(),
            turns: Turns::One,
            generation: 0,
            build: move || async move {
                held.notified().await;
                Ok(5)
            },
        };
        let slow = kept
            .get_within("KORD", build, Duration::from_millis(20))
            .await;
        assert!(matches!(slow, Err(AppError::Busy(STILL_WORKING))));
        release.notify_one();
        bounded(async {
            while kept.len() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let value = kept
            .get(&heavy, Turns::One, "KORD", 0, || async { Ok(99) })
            .await
            .unwrap();
        assert_eq!(value, 5, "kept after the timeout");
        assert_eq!(heavy.free_turns(), HEAVY_TURNS as usize);
    }
}
