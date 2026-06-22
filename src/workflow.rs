use std::time::{Duration, SystemTime};

use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::workflows::{join, join_all};
use temporalio_sdk::{
    ActivityOptions, ApplicationFailure, WorkflowContext, WorkflowContextView, WorkflowResult,
    WorkflowTermination,
};

use crate::{
    ChosenWindow, DiscoverSchedulesInput, FetchWindowsInput, MeasureDurationsInput,
    PlanWindowsInput, PlanWindowsResult, PricedWindow, SECS_PER_DAY, ScheduleRef,
    ScheduleUpdateSummary, SchedulerInput, SchedulerResult, SkippedSchedule, UpdateOutcome,
    UpdateWindowsInput, activities::GridshiftActivities,
};

/// Minimum lead time, in minutes, required before a slot starts for it to be considered
/// schedulable. The workflow still has to complete and commit the updated schedule before
/// the slot begins, so any slot starting within this window of "now" is excluded.
const SCHEDULE_LEAD_MINS: u32 = 2;

fn wf_fail(msg: &str) -> WorkflowTermination {
    WorkflowTermination::failed_application(ApplicationFailure::non_retryable(anyhow::anyhow!(
        "{msg}"
    )))
}

#[workflow]
pub struct SchedulerWorkflow {}

#[workflow_methods]
impl SchedulerWorkflow {
    #[init]
    pub fn new(_ctx: &WorkflowContextView) -> Self {
        Self {}
    }

    #[run]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
        input: SchedulerInput,
    ) -> WorkflowResult<SchedulerResult> {
        let now: SystemTime = ctx
            .workflow_time()
            .ok_or_else(|| wf_fail("no workflow time"))?;

        let tz: Tz = input
            .timezone
            .parse()
            .map_err(|_| wf_fail(&format!("invalid timezone {:?}", input.timezone)))?;

        let now_local = DateTime::<Utc>::from(now).with_timezone(&tz);

        let today = now_local.date_naive();
        let tomorrow = today.succ_opt().expect("date overflow");

        let namespaces: Vec<String> = ctx
            .start_activity(
                GridshiftActivities::list_managed_namespaces,
                (),
                ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
            )
            .await?;

        let discover_outcomes = join_all(namespaces.iter().map(|ns| {
            ctx.start_activity(
                GridshiftActivities::discover_schedules,
                DiscoverSchedulesInput {
                    namespace: ns.clone(),
                    query: input.query.clone(),
                },
                ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
            )
        }))
        .await;

        let mut skipped: Vec<SkippedSchedule> = Vec::new();
        let mut schedules: Vec<_> = Vec::new();
        let mut slot_duration_mins = 30;
        for (ns, outcome) in namespaces.iter().zip(discover_outcomes) {
            match outcome {
                Ok(result) => {
                    slot_duration_mins = result.slot_duration_mins;
                    skipped.extend(result.skipped);
                    schedules.extend(result.schedules);
                }
                Err(e) => skipped.push(SkippedSchedule {
                    namespace: ns.clone(),
                    schedule_id: String::new(),
                    reason: format!("discover_schedules failed: {e:?}"),
                }),
            }
        }

        schedules.sort_by(|a, b| {
            a.namespace
                .cmp(&b.namespace)
                .then(a.schedule_id.cmp(&b.schedule_id))
        });

        if schedules.is_empty() {
            return Ok(SchedulerResult {
                schedules_updated: 0,
                updates: vec![],
                skipped,
            });
        }

        let schedule_refs: Vec<ScheduleRef> = schedules
            .iter()
            .map(|s| ScheduleRef {
                namespace: s.namespace.clone(),
                schedule_id: s.schedule_id.clone(),
            })
            .collect();

        let duration_pairs: Vec<(ScheduleRef, u32)> = ctx
            .start_activity(
                GridshiftActivities::measure_schedule_durations,
                MeasureDurationsInput {
                    schedules: schedule_refs,
                },
                ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
            )
            .await?;

        let (today_result, tomorrow_result) = join!(
            ctx.start_activity(
                GridshiftActivities::fetch_priced_windows,
                FetchWindowsInput {
                    date: today,
                    timezone: input.timezone.clone()
                },
                ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
            ),
            ctx.start_activity(
                GridshiftActivities::try_fetch_priced_windows,
                FetchWindowsInput {
                    date: tomorrow,
                    timezone: input.timezone.clone()
                },
                ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
            ),
        );
        let today_windows: Vec<PricedWindow> = today_result?;
        let tomorrow_windows: Option<Vec<PricedWindow>> = tomorrow_result?.windows;

        let now_mins = now_local.hour() * 60 + now_local.minute();
        let priced_windows: Vec<PricedWindow> = today_windows
            .into_iter()
            .filter(|w| w.date > today || w.hour * 60 + w.minute >= now_mins + SCHEDULE_LEAD_MINS)
            .chain(tomorrow_windows.into_iter().flatten())
            .collect();

        if priced_windows.is_empty() {
            return Ok(SchedulerResult {
                schedules_updated: 0,
                updates: vec![],
                skipped: {
                    let reason = "no price slots available".to_owned();
                    schedules
                        .into_iter()
                        .map(|s| SkippedSchedule {
                            namespace: s.namespace,
                            schedule_id: s.schedule_id,
                            reason: reason.clone(),
                        })
                        .chain(skipped)
                        .collect()
                },
            });
        }

        let now_secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let plan_result: PlanWindowsResult = ctx
            .start_activity(
                GridshiftActivities::plan_window_assignments,
                PlanWindowsInput {
                    schedules,
                    schedule_durations: duration_pairs,
                    priced_windows,
                    timezone: input.timezone.clone(),
                    now_secs,
                    slot_duration_mins,
                },
                ActivityOptions::start_to_close_timeout(Duration::from_secs(30)),
            )
            .await?;

        skipped.extend(plan_result.skipped);

        let dispatched = plan_result.assignments.iter().map(|a| {
            ctx.start_activity(
                GridshiftActivities::update_schedule_windows,
                UpdateWindowsInput {
                    namespace: a.schedule_ref.namespace.clone(),
                    schedule_id: a.schedule_ref.schedule_id.clone(),
                    window: a.window.clone(),
                    timezone: input.timezone.clone(),
                    interval_secs: a.interval_secs,
                },
                ActivityOptions::start_to_close_timeout(Duration::from_secs(60)),
            )
        });
        let outcomes = join_all(dispatched).await;

        let mut updates: Vec<ScheduleUpdateSummary> = Vec::new();
        for (a, outcome) in plan_result.assignments.into_iter().zip(outcomes) {
            match outcome? {
                UpdateOutcome::Skipped { reason } => skipped.push(SkippedSchedule {
                    namespace: a.schedule_ref.namespace,
                    schedule_id: a.schedule_ref.schedule_id,
                    reason,
                }),
                UpdateOutcome::Updated => {
                    let fires_per_day = (SECS_PER_DAY / a.interval_secs).max(1) as i64;
                    let step_mins = (a.interval_secs / 60) as i64;
                    let windows = (0..fires_per_day)
                        .map(|i| {
                            let dt = a
                                .window
                                .date
                                .and_hms_opt(a.window.hour, a.window.minute, 0)
                                .unwrap()
                                + chrono::Duration::minutes(i * step_mins);
                            ChosenWindow {
                                date: dt.date(),
                                hour: dt.hour(),
                                minute: dt.minute(),
                            }
                        })
                        .collect();
                    updates.push(ScheduleUpdateSummary {
                        namespace: a.schedule_ref.namespace,
                        schedule_id: a.schedule_ref.schedule_id,
                        windows,
                    });
                }
            }
        }

        Ok(SchedulerResult {
            schedules_updated: updates.len(),
            updates,
            skipped,
        })
    }
}
