use async_trait::async_trait;
use chrono::{NaiveDate, NaiveTime};
use serde::{Deserialize, Serialize};

pub mod activities;
pub mod providers;
pub mod workflow;

pub const SECS_PER_DAY: u64 = 86_400;

/// Error type returned by [`PriceProvider::fetch_priced_windows`].
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("prices not yet published for {date}")]
    NotYetPublished { date: NaiveDate },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Implemented by energy suppliers. Wired up in the worker binary.
#[async_trait]
pub trait PriceProvider: Send + Sync {
    /// Return all price slots for `date` in chronological order.
    /// Set `weight` to `0.0`; the workflow re-normalizes it from `price_p_per_kwh`.
    /// Returns [`ProviderError::NotYetPublished`] if prices are not yet available for `date`.
    async fn fetch_priced_windows(
        &self,
        date: NaiveDate,
        timezone: &str,
    ) -> Result<Vec<PricedWindow>, ProviderError>;

    /// Duration of each price slot in minutes. Used to convert job durations to slot counts
    /// and to size the minimum price pool. Default: 30 (e.g. Octopus Agile).
    fn slot_duration_mins(&self) -> u32 {
        30
    }

    /// The local time each day when next-day prices are expected to become available.
    /// Returns `None` if prices are available at or before midnight (no wait needed).
    fn next_day_data_available_at(&self) -> Option<NaiveTime> {
        None
    }
}

/// Workflow input, baked into the Temporal Schedule at creation time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerInput {
    /// Temporal visibility query used to discover managed schedules at runtime.
    /// Example: `EnergyIntensive = true`
    pub query: String,
    /// IANA timezone name for this instance (e.g. "Europe/London", "Europe/Paris").
    pub timezone: String,
}

/// A half-hour slot with its price and cheapness weight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricedWindow {
    pub date: NaiveDate,
    pub hour: u32,
    pub minute: u32,
    /// Raw price in pence per kWh (inc. VAT). Negative means the supplier pays the customer.
    pub price_p_per_kwh: f64,
    /// Normalized cheapness weight, computed in the workflow from `price_p_per_kwh`.
    /// Not serialized: it is always recomputed after fetch and is meaningless in transit.
    #[serde(skip)]
    pub weight: f64,
}

/// A single chosen firing time, derived from the cheapest block in a day segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChosenWindow {
    pub date: NaiveDate,
    pub hour: u32,
    pub minute: u32,
}

/// Stable cross-namespace schedule identifier. Schedule IDs are unique only within a namespace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ScheduleRef {
    pub namespace: String,
    pub schedule_id: String,
}

/// Discovered managed interval schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleInfo {
    pub namespace: String,
    pub schedule_id: String,
    /// Interval duration in seconds. gridshift updates only the phase offset within each cycle.
    pub interval_secs: u64,
    /// Epoch seconds of this schedule's most recent actual firing, if it has ever fired.
    /// The workflow uses it to keep a re-phasing from scheduling the next firing less than one
    /// interval after the last run. None means the schedule has no recorded runs yet.
    #[serde(default)]
    pub last_fire_secs: Option<i64>,
}

/// Summary returned by the workflow when it completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerResult {
    pub schedules_updated: usize,
    pub updates: Vec<ScheduleUpdateSummary>,
    /// Schedules that were discovered but not updated, with a short reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedSchedule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedSchedule {
    pub namespace: String,
    pub schedule_id: String,
    pub reason: String,
}

/// Per-schedule summary of chosen firing times.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleUpdateSummary {
    pub namespace: String,
    pub schedule_id: String,
    pub windows: Vec<ChosenWindow>,
}

/// Activity input for fetching priced windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchWindowsInput {
    /// ISO date of the day to fetch prices for.
    pub date: NaiveDate,
    /// IANA timezone name passed through to the provider.
    pub timezone: String,
}

/// Result of try_fetch_priced_windows. Always succeeds; windows is None when prices aren't
/// published yet. The message field is human-readable for the Temporal Web UI result pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TryFetchWindowsResult {
    pub windows: Option<Vec<PricedWindow>>,
    pub message: String,
}

/// Activity input for discovering managed schedules in a single namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverSchedulesInput {
    pub namespace: String,
    /// Raw Temporal visibility query, e.g. `EnergyIntensive = true`.
    pub query: String,
}

/// Activity result for discover_schedules.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoverSchedulesResult {
    pub schedules: Vec<ScheduleInfo>,
    /// Schedules found by the query but skipped (interval, cron, or describe failure).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedSchedule>,
    /// Provider slot duration in minutes (e.g. 30 for Octopus Agile).
    pub slot_duration_mins: u32,
}

/// Activity input for updating a single schedule's phase offset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateWindowsInput {
    pub namespace: String,
    pub schedule_id: String,
    /// The single chosen firing window; gridshift sets the interval phase to match it.
    pub window: ChosenWindow,
    /// IANA timezone name used to convert the window's wall-clock time to UTC.
    /// The schedule's own ScheduleSpec.timezone_name is preserved.
    pub timezone: String,
    /// Interval duration in seconds; the phase is set as utc_timestamp % interval_secs.
    pub interval_secs: u64,
}

/// Outcome of an update_schedule_windows activity call.
/// A schedule the worker can read but lacks permission to update comes back as `Skipped`
/// so the workflow routes it to its skipped list instead of counting it as a successful
/// update; otherwise the result would over-report what changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UpdateOutcome {
    Updated,
    Skipped { reason: String },
}

/// Activity input for measuring historical runtimes of managed workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasureDurationsInput {
    pub schedules: Vec<ScheduleRef>,
}
