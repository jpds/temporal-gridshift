use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::TimeZone;
use futures::stream::{self, StreamExt};
use temporalio_client::{
    Client, ClientOptions, Connection, WorkflowListOptions,
    schedules::{ListSchedulesOptions, ScheduleError, ScheduleIntervalSpec, ScheduleSpec},
};
use temporalio_common::protos::temporal::api::workflowservice::v1::ListNamespacesRequest;
use temporalio_macros::activities;
use temporalio_sdk::activities::{ActivityContext, ActivityError};
use tonic::IntoRequest;

use crate::{
    DiscoverSchedulesInput, DiscoverSchedulesResult, FetchWindowsInput, MeasureDurationsInput,
    PlanWindowsInput, PlanWindowsResult, PriceProvider, PricedWindow, ProviderError, ScheduleInfo,
    ScheduleRef, SkippedSchedule, TryFetchWindowsResult, UpdateOutcome, UpdateWindowsInput,
};

use tracing::warn;

/// Outcome of probing one schedule during discovery: its interval seconds and last-fire epoch
/// seconds, or a human-readable reason the schedule was skipped.
type ScheduleProbe = Result<(u64, Option<i64>), String>;

pub struct GridshiftActivities {
    pub connection: Arc<Connection>,
    pub client_opts: ClientOptions,
    pub provider: Arc<dyn PriceProvider>,
}

impl GridshiftActivities {
    fn client_for(&self, namespace: &str) -> Result<Client, ActivityError> {
        let mut opts = self.client_opts.clone();
        opts.namespace = namespace.to_string();
        Client::new((*self.connection).clone(), opts)
            .map_err(|e| ActivityError::from(anyhow::anyhow!("{e}")))
    }
}

#[activities]
impl GridshiftActivities {
    /// Return all namespace names visible to this client via the cluster-scoped ListNamespaces RPC.
    /// Requires the System Reader role (or higher); all namespaces on the server are returned.
    #[activity]
    pub async fn list_managed_namespaces(
        self: Arc<Self>,
        _ctx: ActivityContext,
    ) -> Result<Vec<String>, ActivityError> {
        let mut service = self.connection.workflow_service();
        let mut namespaces = Vec::new();
        let mut next_page_token = vec![];

        loop {
            let resp = service
                .list_namespaces(
                    ListNamespacesRequest {
                        page_size: 100,
                        next_page_token: next_page_token.clone(),
                        ..Default::default()
                    }
                    .into_request(),
                )
                .await
                .map_err(|e| ActivityError::from(anyhow::anyhow!("list_namespaces: {e}")))?
                .into_inner();

            for ns in resp.namespaces {
                if let Some(info) = ns.namespace_info
                    && !info.name.is_empty()
                    && info.name != "temporal-system"
                {
                    namespaces.push(info.name);
                }
            }

            next_page_token = resp.next_page_token;
            if next_page_token.is_empty() {
                break;
            }
        }

        Ok(namespaces)
    }

    /// Discover managed schedules in a single namespace via a free-form visibility query.
    /// On permission error or missing search attribute - logs a warning and returns empty vecs
    /// rather than failing the activity, so one inaccessible namespace does not block others.
    #[activity]
    pub async fn discover_schedules(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: DiscoverSchedulesInput,
    ) -> Result<DiscoverSchedulesResult, ActivityError> {
        let client = self.client_for(&input.namespace)?;

        let mut ids = Vec::new();
        let mut sched_stream = client.list_schedules(
            ListSchedulesOptions::builder()
                .query(input.query.clone())
                .build(),
        );
        while let Some(entry) = sched_stream.next().await {
            match entry {
                Ok(e) => ids.push(e.schedule_id().to_owned()),
                Err(e) => {
                    // A permanent error here means that this namespace is unmanageable for this
                    // search query
                    let is_permanent = match &e {
                        ScheduleError::Rpc(s) => matches!(
                            s.code(),
                            tonic::Code::InvalidArgument
                                | tonic::Code::NotFound
                                | tonic::Code::PermissionDenied
                        ),
                        _ => false,
                    };

                    if is_permanent {
                        warn!(
                            namespace = %input.namespace,
                            error = %e,
                            "skipping namespace: list_schedules failed permanently"
                        );

                        return Ok(DiscoverSchedulesResult {
                            slot_duration_mins: self.provider.slot_duration_mins(),
                            skipped: vec![SkippedSchedule {
                                namespace: input.namespace.clone(),
                                schedule_id: String::new(),
                                reason: format!("list_schedules: {e}"),
                            }],
                            ..Default::default()
                        });
                    }

                    return Err(ActivityError::from(anyhow::anyhow!(
                        "list_schedules {}: {e}",
                        input.namespace
                    )));
                }
            }
        }

        let outcomes: Vec<(String, ScheduleProbe)> = stream::iter(ids)
            .map(|schedule_id| {
                // Every schedule here shares input.namespace - create a single client
                let client = client.clone();

                async move {
                    let outcome: ScheduleProbe = match client
                        .get_schedule_handle(schedule_id.clone())
                        .describe()
                        .await
                    {
                        Ok(desc) => {
                            // Timestamp of the last actual firing. Used to ensure we don't re-phase
                            // an interval schedule until at least one full interval has elapsed.
                            let last_fire_secs = desc
                                .recent_actions()
                                .iter()
                                .filter_map(|a| a.actual_time)
                                .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64)
                                .max();
                            let update = desc.into_update();
                            let raw = update.raw();
                            match raw.spec.as_ref().and_then(|s| s.interval.first()) {
                                Some(iv) => {
                                    // Clamp negative proto durations to zero to avoid wrapping to a
                                    // huge u64.
                                    let secs =
                                        iv.interval.as_ref().map(|d| d.seconds.max(0)).unwrap_or(0)
                                            as u64;

                                    if secs == 0 {
                                        Err("interval is zero".to_owned())
                                    } else {
                                        Ok((secs, last_fire_secs))
                                    }
                                }
                                None => Err("not an interval schedule".to_owned()),
                            }
                        }
                        Err(e) => Err(format!("describe failed: {e}")),
                    };
                    (schedule_id, outcome)
                }
            })
            .buffer_unordered(10)
            .collect()
            .await;

        let mut result = DiscoverSchedulesResult {
            slot_duration_mins: self.provider.slot_duration_mins(),
            ..Default::default()
        };
        for (schedule_id, outcome) in outcomes {
            match outcome {
                Ok((interval_secs, last_fire_secs)) => result.schedules.push(ScheduleInfo {
                    namespace: input.namespace.clone(),
                    schedule_id,
                    interval_secs,
                    last_fire_secs,
                }),
                Err(reason) => {
                    warn!(namespace = %input.namespace, schedule_id, reason, "skipping schedule");
                    result.skipped.push(SkippedSchedule {
                        namespace: input.namespace.clone(),
                        schedule_id,
                        reason,
                    });
                }
            }
        }

        // Sort by schedule ID to make slot assignment deterministic. Without this,
        // concurrent discovery would assign the cheapest slots to whichever describes
        // completed first.
        result
            .schedules
            .sort_by(|a, b| a.schedule_id.cmp(&b.schedule_id));

        Ok(result)
    }

    /// Fetch all priced windows for a day from the configured provider.
    #[activity]
    pub async fn fetch_priced_windows(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: FetchWindowsInput,
    ) -> Result<Vec<PricedWindow>, ActivityError> {
        self.provider
            .fetch_priced_windows(input.date, &input.timezone)
            .await
            .map_err(|e| ActivityError::from(anyhow::Error::from(e)))
    }

    /// Like fetch_priced_windows but treats "not yet published" as a non-error outcome.
    /// Returns a result with a human-readable message.
    #[activity]
    pub async fn try_fetch_priced_windows(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: FetchWindowsInput,
    ) -> Result<TryFetchWindowsResult, ActivityError> {
        match self
            .provider
            .fetch_priced_windows(input.date, &input.timezone)
            .await
        {
            Ok(windows) => Ok(TryFetchWindowsResult {
                message: format!("Fetched {} slots for {}", windows.len(), input.date),
                windows: Some(windows),
            }),
            Err(ProviderError::NotYetPublished { .. }) => Ok(TryFetchWindowsResult {
                message: format!("Prices not yet published for {}", input.date),
                windows: None,
            }),
            Err(e) => Err(ActivityError::from(anyhow::Error::from(e))),
        }
    }

    /// Assign each eligible schedule to a price window and return the full assignment plan.
    /// Schedules that share a window fire concurrently, so the window covers the longest job.
    /// Groups form greedily longest-first; schedules beyond the price horizon come back as skipped.
    #[activity]
    pub async fn plan_window_assignments(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: PlanWindowsInput,
    ) -> Result<PlanWindowsResult, ActivityError> {
        let tz: chrono_tz::Tz = input.timezone.parse().map_err(|_| {
            ActivityError::from(anyhow::anyhow!("invalid timezone: {:?}", input.timezone))
        })?;

        let schedule_durations: std::collections::HashMap<ScheduleRef, u32> =
            input.schedule_durations.into_iter().collect();

        Ok(crate::planning::plan_assignments(
            input.schedules,
            &schedule_durations,
            input.priced_windows,
            &tz,
            input.now_secs,
            input.slot_duration_mins,
        ))
    }

    /// Update a managed interval schedule's phase to fire at the chosen cheap window.
    /// One activity call per schedule for observability in the workflow event history.
    #[activity]
    pub async fn update_schedule_windows(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: UpdateWindowsInput,
    ) -> Result<UpdateOutcome, ActivityError> {
        let client = self.client_for(&input.namespace)?;

        let tz: chrono_tz::Tz = input.timezone.parse().map_err(|_| {
            ActivityError::from(anyhow::anyhow!("invalid timezone: {:?}", input.timezone))
        })?;
        let window = input.window.clone();
        let ndt = window
            .date
            .and_hms_opt(window.hour, window.minute, 0)
            .ok_or_else(|| {
                ActivityError::from(anyhow::anyhow!(
                    "invalid window {:02}:{:02}",
                    window.hour,
                    window.minute
                ))
            })?;
        let utc_ts: u64 = tz
            .from_local_datetime(&ndt)
            .earliest()
            .and_then(|dt| u64::try_from(dt.timestamp()).ok())
            .unwrap_or(0);

        match client
            .get_schedule_handle(input.schedule_id.clone())
            .update(move |u| {
                let raw_spec = u.raw().spec.clone();
                let s = raw_spec.as_ref();

                // Preserve the schedule's own timezone. gridshift only shifts the interval phase
                // (computed in UTC above), so it must not rewrite the user-configured
                // timezone_name. An empty string keeps Temporal's default of UTC.
                let timezone_name = s.map(|spec| spec.timezone_name.clone()).unwrap_or_default();

                let jitter = s
                    .and_then(|spec| spec.jitter.as_ref())
                    .map(|d| StdDuration::from_secs(d.seconds.max(0) as u64));

                let ts_to_st = |secs: i64, nanos: i32| -> Option<std::time::SystemTime> {
                    if secs < 0 {
                        return None;
                    }
                    let d = StdDuration::from_secs(secs as u64)
                        .checked_add(StdDuration::from_nanos(nanos.max(0) as u64))?;
                    Some(std::time::SystemTime::UNIX_EPOCH + d)
                };
                let start_time = s
                    .and_then(|spec| spec.start_time.as_ref())
                    .and_then(|ts| ts_to_st(ts.seconds, ts.nanos));
                let end_time = s
                    .and_then(|spec| spec.end_time.as_ref())
                    .and_then(|ts| ts_to_st(ts.seconds, ts.nanos));

                // Temporal evaluates interval firings as (epoch + n*every + phase) in UTC. Take the
                // chosen window's UTC epoch (utc_ts above) modulo each interval; this guarantees
                // phase < every, satisfying the server's validation constraint.
                let intervals: Vec<ScheduleIntervalSpec> = s
                    .map(|spec| {
                        spec.interval
                            .iter()
                            .filter_map(|iv| {
                                let every_secs = iv.interval.as_ref()?.seconds.max(0) as u64;
                                let phase = if every_secs > 0 {
                                    utc_ts % every_secs
                                } else {
                                    0
                                };
                                Some(ScheduleIntervalSpec::new(
                                    StdDuration::from_secs(every_secs),
                                    Some(StdDuration::from_secs(phase)),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let mut spec = ScheduleSpec::builder()
                    .intervals(intervals)
                    .timezone_name(timezone_name)
                    .build();
                spec.jitter = jitter;
                spec.start_time = start_time;
                spec.end_time = end_time;
                u.set_spec(spec);
            })
            .await
        {
            Ok(()) => Ok(UpdateOutcome::Updated),
            Err(ScheduleError::Rpc(status)) if status.code() == tonic::Code::PermissionDenied => {
                warn!(
                    namespace = %input.namespace,
                    schedule_id = %input.schedule_id,
                    "permission denied updating schedule; skipping"
                );
                Ok(UpdateOutcome::Skipped {
                    reason: "permission denied updating schedule".to_owned(),
                })
            }
            Err(e) => Err(ActivityError::from(e)),
        }
    }

    /// Query Temporal visibility for recent completed runs of each managed schedule.
    /// Returns the p95 duration per schedule, rounded up to the nearest provider slot.
    /// Schedules with no recorded completions default to one slot.
    ///
    /// Returns Vec rather than HashMap because HashMap<ScheduleRef, u32> cannot round-trip
    /// through Temporal's JSON data converter (JSON object keys must be strings). The workflow
    /// rebuilds an in-memory HashMap<ScheduleRef, u32> from this Vec for lookup.
    #[activity]
    pub async fn measure_schedule_durations(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: MeasureDurationsInput,
    ) -> Result<Vec<(ScheduleRef, u32)>, ActivityError> {
        let slot_mins = self.provider.slot_duration_mins();

        // Schedules here can span many namespaces, so build at most one client per distinct
        // namespace up front rather than one per schedule. A namespace whose client cannot be
        // built is omitted; its schedules fall back to one slot below.
        let mut clients: std::collections::HashMap<String, Client> =
            std::collections::HashMap::new();
        for sched_ref in &input.schedules {
            if !clients.contains_key(&sched_ref.namespace) {
                match self.client_for(&sched_ref.namespace) {
                    Ok(c) => {
                        clients.insert(sched_ref.namespace.clone(), c);
                    }
                    Err(e) => warn!(
                        namespace = %sched_ref.namespace,
                        error = ?e,
                        "failed to build client, defaulting its schedules to one slot"
                    ),
                }
            }
        }

        let pairs: Vec<(ScheduleRef, u32)> = stream::iter(input.schedules)
            .map(|sched_ref| {
                let client = clients.get(&sched_ref.namespace).cloned();
                async move {
                    let Some(client) = client else {
                        return (sched_ref, slot_mins);
                    };

                    let query = format!(
                        "TemporalScheduledById = \"{}\" AND ExecutionStatus = \"Completed\"",
                        sched_ref.schedule_id
                    );
                    // Temporal visibility is eventually consistent; a just-completed run may not
                    // appear yet, causing a slight undercount of recent history.
                    let mut wf_stream = client
                        .list_workflows(query, WorkflowListOptions::builder().limit(20).build());

                    let mut durations_secs: Vec<u64> = Vec::new();
                    while let Some(exec) = wf_stream.next().await {
                        // Swallow per-item errors to avoid aborting the whole batch.
                        if let Ok(exec) = exec
                            && let (Some(start), Some(end)) = (exec.start_time(), exec.close_time())
                            && let Ok(duration) = end.duration_since(start)
                        {
                            durations_secs.push(duration.as_secs());
                        }
                    }

                    let mins = if durations_secs.is_empty() {
                        slot_mins
                    } else {
                        durations_secs.sort_unstable();
                        // Nearest-rank p95: ceil(0.95 * n) - 1, clamped into range. Truncating
                        // instead of ceiling would round down to the maximum sample at n = 20.
                        let p95_idx = (((durations_secs.len() as f64) * 0.95).ceil() as usize)
                            .saturating_sub(1)
                            .min(durations_secs.len() - 1);
                        let p95_secs = durations_secs[p95_idx];
                        let mins = p95_secs.div_ceil(60) as u32;
                        u32::div_ceil(mins, slot_mins) * slot_mins
                    };
                    (sched_ref, mins.max(slot_mins))
                }
            })
            .buffer_unordered(10)
            .collect()
            .await;

        Ok(pairs)
    }
}
