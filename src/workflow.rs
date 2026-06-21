use std::time::{Duration, SystemTime};

use chrono::{DateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::workflows::{join, join_all};
use temporalio_sdk::{
    ActivityOptions, ApplicationFailure, WorkflowContext, WorkflowContextView, WorkflowResult,
    WorkflowTermination,
};

use crate::{
    ChosenWindow, DiscoverSchedulesInput, FetchWindowsInput, MeasureDurationsInput, PricedWindow,
    SECS_PER_DAY, ScheduleRef, ScheduleUpdateSummary, SchedulerInput, SchedulerResult,
    SkippedSchedule, UpdateOutcome, UpdateWindowsInput, activities::GridshiftActivities,
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

/// Converts a slot's local wall-clock start time to a UTC Unix timestamp. Using chrono-tz is
/// deterministic in workflow code because its IANA time-zone database is compiled into the
/// binary. If the local time falls in a DST spring-forward gap, no timestamp exists; in that case
/// `i64::MIN` is returned so the interval guard treats the slot as ineligible.
fn slot_epoch(w: &PricedWindow, tz: &Tz) -> i64 {
    let ndt = w.date.and_hms_opt(w.hour, w.minute, 0).unwrap();
    tz.from_local_datetime(&ndt)
        .earliest()
        .map(|dt| dt.timestamp())
        .unwrap_or(i64::MIN)
}

/// Returns the index of the first slot that can be scheduled without allowing the next firing to
/// occur less than one interval after the previous one.
///
/// Interval schedules fire at `epoch + n * interval + phase`. Moving the phase earlier can pull
/// the next firing into the previous interval, so only slots at or after `last_fire + interval`
/// are considered. Since `priced` is ordered chronologically, the eligible slots form a contiguous
/// suffix and this returns its first index (or `priced.len()` if none qualify).
///
/// This check is exact for intervals of one day or longer. For shorter intervals, changing the
/// phase also affects earlier firings on the same day, which this slot-level guard does not model.
fn earliest_eligible_index(
    priced: &[PricedWindow],
    last_fire_secs: Option<i64>,
    interval_secs: u64,
    tz: &Tz,
) -> usize {
    let Some(last_fire) = last_fire_secs else {
        return 0;
    };
    let floor = last_fire + interval_secs as i64;
    priced
        .iter()
        .position(|w| slot_epoch(w, tz) >= floor)
        .unwrap_or(priced.len())
}

/// Human-readable reason for skipping a schedule whose first eligible firing lies beyond the price
/// horizon. The only way no slot qualifies is that `last_fire + interval` is further out than
/// the prices we hold, which is the normal state for an interval longer than a day or two. Report
/// the countdown to the next run so the schedule reads as "waiting for prices", not "blocked".
/// `now_secs` is the workflow's current time in epoch seconds.
fn horizon_skip_reason(last_fire_secs: Option<i64>, interval_secs: u64, now_secs: i64) -> String {
    match last_fire_secs {
        Some(last_fire) => {
            let floor = last_fire + interval_secs as i64;
            let remaining = std::time::Duration::from_secs((floor - now_secs).max(0) as u64);
            format!(
                "next run in {}, outside the price horizon",
                humantime::format_duration(remaining)
            )
        }
        None => "no eligible price slot in the price horizon".to_owned(),
    }
}

/// Divide the day into `fires_per_day` equal segments and find the cheapest contiguous
/// block of `duration_slots` half-hour slots within each segment.
/// Returns one `(ChosenWindow, slot_index)` per segment, where `slot_index` is the chosen
/// block's start position in `priced`. The caller uses the index to mark occupied slots
/// without re-scanning the pool.
///
/// The workflow calls this with `fires_per_day = 1`, so each schedule claims the single
/// cheapest block in the pool and `mark_claimed` then steers later schedules away from it. The
/// multi-segment path exists for the unit tests and any future caller that wants several
/// non-overlapping blocks from one pool.
fn select_windows(
    priced: &[PricedWindow],
    fires_per_day: usize,
    duration_slots: usize,
) -> Vec<(ChosenWindow, usize)> {
    if priced.is_empty() || fires_per_day == 0 {
        return vec![];
    }
    let fires = fires_per_day.min(priced.len());
    let duration = duration_slots.max(1);
    let segment_len = priced.len() / fires;

    // Prefix sums for O(1) block scoring: weight drives the primary score (higher = cheaper),
    // raw price the tiebreaker. Entry k is the sum of the first k slots, so the sum over
    // [start, end) is `prefix[end] - prefix[start]`.
    let weight_prefix: Vec<f64> = std::iter::once(0.0)
        .chain(priced.iter().scan(0.0, |acc, w| {
            *acc += w.weight;
            Some(*acc)
        }))
        .collect();
    let price_prefix: Vec<f64> = std::iter::once(0.0)
        .chain(priced.iter().scan(0.0, |acc, w| {
            *acc += w.price_p_per_kwh;
            Some(*acc)
        }))
        .collect();

    (0..fires)
        .map(|i| {
            let seg_start = i * segment_len;
            let seg_end = if i == fires - 1 {
                priced.len()
            } else {
                seg_start + segment_len
            };
            // Last possible start so the window fits; fall back to seg_start if window > segment.
            let last_start = seg_end.saturating_sub(duration).max(seg_start);

            let best = (seg_start..=last_start)
                .max_by(|&a, &b| {
                    let end_a = (a + duration).min(priced.len());
                    let end_b = (b + duration).min(priced.len());
                    let wa = weight_prefix[end_a] - weight_prefix[a];
                    let wb = weight_prefix[end_b] - weight_prefix[b];
                    // When weights are equal (including all-zero after slot exhaustion),
                    // prefer the block with the lower raw price as a tiebreaker.
                    wa.total_cmp(&wb).then_with(|| {
                        let price_a = price_prefix[end_a] - price_prefix[a];
                        let price_b = price_prefix[end_b] - price_prefix[b];
                        price_b.total_cmp(&price_a)
                    })
                })
                .unwrap_or(seg_start);

            (
                ChosenWindow {
                    date: priced[best].date,
                    hour: priced[best].hour,
                    minute: priced[best].minute,
                },
                best,
            )
        })
        .collect()
}

/// Re-normalize each slot's cheapness weight across the pool: the cheapest slot becomes 1.0,
/// the most expensive 0.0, scaled linearly by price in between. When every slot has the same
/// price the range is zero and all weights are set to 1.0. Any incoming weight is overwritten.
fn normalize_weights(priced: &mut [PricedWindow]) {
    let min_price = priced
        .iter()
        .map(|w| w.price_p_per_kwh)
        .fold(f64::INFINITY, f64::min);
    let max_price = priced
        .iter()
        .map(|w| w.price_p_per_kwh)
        .fold(f64::NEG_INFINITY, f64::max);
    let price_range = max_price - min_price;
    for w in priced.iter_mut() {
        w.weight = if price_range > 0.0 {
            (max_price - w.price_p_per_kwh) / price_range
        } else {
            1.0
        };
    }
}

/// Mark a claimed block so later schedules steer away from it, starting at `start` and
/// spanning `duration_slots` (clamped to the pool end). A multi-slot job needs the whole
/// contiguous block, so those slots are zeroed outright; a single-slot job only halves its
/// slot's weight, leaving a very cheap slot still attractive if nothing else competes for it.
fn mark_claimed(priced: &mut [PricedWindow], start: usize, duration_slots: usize) {
    let end = (start + duration_slots).min(priced.len());
    for w in &mut priced[start..end] {
        if duration_slots > 1 {
            w.weight = 0.0;
        } else {
            w.weight *= 0.5;
        }
    }
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

        // Fan out one discover_schedules per namespace. join_all keeps ordering deterministic
        // across replay.
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

        // discover_schedules already turns permanent errors into an in-result skip, record
        // transient errors as a skip here too - so one unreachable namespace does not abort
        // scheduling for every other namespace.
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

        // Stable sort by (namespace, schedule_id) so slot assignment is deterministic on replay.
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

        let schedule_durations: std::collections::HashMap<ScheduleRef, u32> =
            duration_pairs.into_iter().collect();

        // Fetch today and tomorrow in parallel.
        // Tomorrow's prices may not be published yet if the workflow is triggered before the
        // provider's daily refresh (normally ~16:00).
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
        let mut priced_windows: Vec<PricedWindow> = today_windows
            .into_iter()
            .filter(|w| w.date > today || w.hour * 60 + w.minute >= now_mins + SCHEDULE_LEAD_MINS)
            .chain(tomorrow_windows.into_iter().flatten())
            .collect();

        // Re-normalize weights across the remaining scheduling window. The per-day weights were calculated
        // before past slots were discarded, so they no longer reflect the available pool.
        // Recomputing here ensures the remaining slots today and tomorrow are ranked consistently
        // against each other.
        normalize_weights(&mut priced_windows);

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

        // Window selection and slot-marking must remain sequential: each schedule zeroes out
        // the weights it claims so later schedules avoid those slots.
        let mut pending: Vec<(ScheduleRef, ChosenWindow, u64)> = Vec::new();

        let now_secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        for info in schedules {
            let sched_ref = ScheduleRef {
                namespace: info.namespace.clone(),
                schedule_id: info.schedule_id.clone(),
            };
            let window_duration_mins = schedule_durations
                .get(&sched_ref)
                .copied()
                .unwrap_or(slot_duration_mins);
            let duration_slots = window_duration_mins.div_ceil(slot_duration_mins).max(1) as usize;

            // Skip any slots that would fire this schedule again within one interval of its last
            // run; see earliest_eligible_index. When nothing is left, leave the schedule on its
            // existing spec rather than pulling its next firing forward.
            let min_start = earliest_eligible_index(
                &priced_windows,
                info.last_fire_secs,
                info.interval_secs,
                &tz,
            );
            if min_start >= priced_windows.len() {
                skipped.push(SkippedSchedule {
                    namespace: info.namespace,
                    schedule_id: info.schedule_id,
                    reason: horizon_skip_reason(info.last_fire_secs, info.interval_secs, now_secs),
                });
                continue;
            }

            let selected = select_windows(&priced_windows[min_start..], 1, duration_slots);

            for &(_, start) in &selected {
                mark_claimed(&mut priced_windows, min_start + start, duration_slots);
            }

            if let Some((window, _)) = selected.into_iter().next() {
                pending.push((sched_ref, window, info.interval_secs));
            }
        }

        // Dispatch update_schedule_windows concurrently. join_all polls the futures in a fixed
        // declaration order, keeping command generation deterministic across workflow replay.
        let dispatched = pending.iter().map(|(sched_ref, window, interval_secs)| {
            ctx.start_activity(
                GridshiftActivities::update_schedule_windows,
                UpdateWindowsInput {
                    namespace: sched_ref.namespace.clone(),
                    schedule_id: sched_ref.schedule_id.clone(),
                    window: window.clone(),
                    timezone: input.timezone.clone(),
                    interval_secs: *interval_secs,
                },
                ActivityOptions::start_to_close_timeout(Duration::from_secs(60)),
            )
        });
        let outcomes = join_all(dispatched).await;

        // Pair each dispatch result with its schedule. A schedule the worker can read but lacks
        // permission to update returns UpdateOutcome::Skipped; route it to `skipped` rather than
        // counting it as a successful update. Activity failures still propagate via `?`.
        let mut updates: Vec<ScheduleUpdateSummary> = Vec::new();
        for ((sched_ref, window, interval_secs), outcome) in pending.into_iter().zip(outcomes) {
            match outcome? {
                UpdateOutcome::Skipped { reason } => skipped.push(SkippedSchedule {
                    namespace: sched_ref.namespace,
                    schedule_id: sched_ref.schedule_id,
                    reason,
                }),
                UpdateOutcome::Updated => {
                    let fires_per_day = (SECS_PER_DAY / interval_secs).max(1) as i64;
                    let step_mins = (interval_secs / 60) as i64;
                    let windows = (0..fires_per_day)
                        .map(|i| {
                            let dt = window
                                .date
                                .and_hms_opt(window.hour, window.minute, 0)
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
                        namespace: sched_ref.namespace,
                        schedule_id: sched_ref.schedule_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn slot(hour: u32, minute: u32, price: f64, weight: f64) -> PricedWindow {
        PricedWindow {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            hour,
            minute,
            price_p_per_kwh: price,
            weight,
        }
    }

    /// Collapse a result into `(hour, minute, start_index)` triples for terse assertions.
    fn hms(result: &[(ChosenWindow, usize)]) -> Vec<(u32, u32, usize)> {
        result
            .iter()
            .map(|(w, idx)| (w.hour, w.minute, *idx))
            .collect()
    }

    #[test]
    fn empty_pool_returns_empty() {
        assert!(select_windows(&[], 1, 1).is_empty());
    }

    #[test]
    fn zero_fires_returns_empty() {
        assert!(select_windows(&[slot(2, 0, 5.0, 1.0)], 0, 1).is_empty());
    }

    #[test]
    fn picks_highest_weight_slot() {
        let pool = vec![
            slot(0, 0, 30.0, 0.0),
            slot(0, 30, 30.0, 0.0),
            slot(2, 0, 5.0, 1.0),
            slot(2, 30, 30.0, 0.0),
        ];
        let [(w, _)] = select_windows(&pool, 1, 1).try_into().unwrap();
        assert_eq!((w.hour, w.minute), (2, 0));
    }

    #[test]
    fn picks_cheapest_contiguous_block_for_duration() {
        let pool = vec![
            slot(0, 0, 5.0, 1.0),
            slot(0, 30, 5.0, 1.0),
            slot(1, 0, 30.0, 0.0),
            slot(1, 30, 30.0, 0.0),
        ];
        let [(w, idx)] = select_windows(&pool, 1, 2).try_into().unwrap();
        assert_eq!((w.hour, w.minute), (0, 0));
        assert_eq!(idx, 0);
    }

    #[test]
    fn tiebreaker_prefers_lower_raw_price() {
        let pool = vec![
            slot(0, 0, 20.0, 0.0),
            slot(0, 30, 10.0, 0.0),
            slot(1, 0, 20.0, 0.0),
        ];
        let [(w, _)] = select_windows(&pool, 1, 1).try_into().unwrap();
        assert_eq!((w.hour, w.minute), (0, 30));
    }

    #[test]
    fn single_slot_pool() {
        let [(w, idx)] = select_windows(&[slot(3, 0, 5.0, 1.0)], 1, 1)
            .try_into()
            .unwrap();
        assert_eq!((w.hour, w.minute, idx), (3, 0, 0));
    }

    #[test]
    fn duration_larger_than_pool_clamps_to_start() {
        let pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        let [(_, idx)] = select_windows(&pool, 1, 10).try_into().unwrap();
        assert_eq!(idx, 0);
    }

    #[test]
    fn zeroed_weight_steers_later_selection_away() {
        // Mimics the workflow's slot-marking: the first schedule claims the cheapest slot and
        // zeroes its weight, so a re-selection must fall through to the next-cheapest slot.
        let mut pool = vec![
            slot(2, 0, 5.0, 1.0),
            slot(2, 30, 8.0, 0.5),
            slot(3, 0, 30.0, 0.0),
        ];
        let [(_, first_idx)] = select_windows(&pool, 1, 1).try_into().unwrap();
        assert_eq!(first_idx, 0);
        pool[first_idx].weight = 0.0;
        let [(w2, idx2)] = select_windows(&pool, 1, 1).try_into().unwrap();
        assert_eq!((w2.hour, w2.minute, idx2), (2, 30, 1));
    }

    #[test]
    fn fires_exceeding_pool_clamp_to_one_window_per_slot() {
        // `fires_per_day` is clamped to the pool length, yielding one firing per slot.
        let pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        let result = select_windows(&pool, 5, 1);
        assert_eq!(hms(&result), vec![(0, 0, 0), (0, 30, 1)]);
    }

    #[test]
    fn last_segment_absorbs_remainder_slots() {
        // 5 slots / 2 fires => segment_len 2, so the final segment spans [2, 5). The cheapest
        // slot at index 4 is only reachable if that segment really extends to the pool's end.
        let pool = vec![
            slot(0, 0, 30.0, 0.0),
            slot(0, 30, 5.0, 1.0),
            slot(1, 0, 30.0, 0.0),
            slot(1, 30, 30.0, 0.0),
            slot(2, 0, 5.0, 1.0),
        ];
        let result = select_windows(&pool, 2, 1);
        assert_eq!(hms(&result), vec![(0, 30, 1), (2, 0, 4)]);
    }

    #[test]
    fn multiple_fires_with_multi_slot_duration() {
        // 8 slots / 2 fires, each firing needs a contiguous 2-slot block. The cheapest block in
        // segment 0 sits at its start, in segment 1 at its end.
        let pool = vec![
            slot(0, 0, 5.0, 1.0),
            slot(0, 30, 5.0, 1.0),
            slot(1, 0, 30.0, 0.0),
            slot(1, 30, 30.0, 0.0),
            slot(2, 0, 30.0, 0.0),
            slot(2, 30, 30.0, 0.0),
            slot(3, 0, 5.0, 1.0),
            slot(3, 30, 5.0, 1.0),
        ];
        let result = select_windows(&pool, 2, 2);
        assert_eq!(hms(&result), vec![(0, 0, 0), (3, 0, 6)]);
    }

    #[test]
    fn normalize_weights_maps_cheapest_to_one_dearest_to_zero() {
        // Incoming weights are deliberately wrong to prove they get overwritten.
        let mut pool = vec![
            slot(0, 0, 10.0, 9.9),
            slot(0, 30, 20.0, 9.9),
            slot(1, 0, 30.0, 9.9),
        ];
        normalize_weights(&mut pool);
        let weights: Vec<f64> = pool.iter().map(|w| w.weight).collect();
        assert_eq!(weights, vec![1.0, 0.5, 0.0]);
    }

    #[test]
    fn normalize_weights_handles_negative_prices() {
        // Negative prices (supplier pays the customer) are the cheapest, so weight 1.0.
        let mut pool = vec![slot(0, 0, -5.0, 0.0), slot(0, 30, 15.0, 0.0)];
        normalize_weights(&mut pool);
        assert_eq!(pool[0].weight, 1.0);
        assert_eq!(pool[1].weight, 0.0);
    }

    #[test]
    fn normalize_weights_flat_prices_all_one() {
        // Zero price range: every slot is equally cheap, so all weights are 1.0.
        let mut pool = vec![slot(0, 0, 12.0, 0.0), slot(0, 30, 12.0, 0.0)];
        normalize_weights(&mut pool);
        assert!(pool.iter().all(|w| w.weight == 1.0));
    }

    #[test]
    fn mark_claimed_zeroes_multi_slot_block() {
        // A multi-slot job claims its whole contiguous block, zeroing those weights only.
        let mut pool = vec![
            slot(0, 0, 5.0, 1.0),
            slot(0, 30, 5.0, 1.0),
            slot(1, 0, 5.0, 1.0),
            slot(1, 30, 5.0, 1.0),
        ];
        mark_claimed(&mut pool, 1, 2);
        let weights: Vec<f64> = pool.iter().map(|w| w.weight).collect();
        assert_eq!(weights, vec![1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn mark_claimed_halves_single_slot() {
        // A single-slot job only halves its slot, so a very cheap slot can still be picked again.
        let mut pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 0.8)];
        mark_claimed(&mut pool, 0, 1);
        assert_eq!(pool[0].weight, 0.5);
        assert_eq!(pool[1].weight, 0.8);
    }

    #[test]
    fn mark_claimed_clamps_block_to_pool_end() {
        // A block running past the pool end clamps rather than panicking.
        let mut pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        mark_claimed(&mut pool, 1, 5);
        assert_eq!(pool[1].weight, 0.0);
    }

    #[test]
    fn block_does_not_cross_segment_boundary() {
        // A cheap pair straddles the segment boundary (indices 2,3). Because a block may not
        // cross into the next segment, neither firing can claim the pair whole: segment 0 grabs
        // only its near half (index 2 via start 1) and segment 1 grabs only its half (index 3).
        let pool = vec![
            slot(0, 0, 30.0, 0.0),
            slot(0, 30, 30.0, 0.0),
            slot(1, 0, 5.0, 1.0),
            slot(1, 30, 5.0, 1.0),
            slot(2, 0, 30.0, 0.0),
            slot(2, 30, 30.0, 0.0),
        ];
        let result = select_windows(&pool, 2, 2);
        assert_eq!(hms(&result), vec![(0, 30, 1), (1, 30, 3)]);
    }

    #[test]
    fn segments_distribute_across_day() {
        let pool = vec![
            slot(0, 0, 30.0, 0.0),
            slot(0, 30, 5.0, 1.0),
            slot(1, 0, 30.0, 0.0),
            slot(1, 30, 30.0, 0.0),
            slot(2, 0, 5.0, 1.0),
            slot(2, 30, 30.0, 0.0),
        ];
        let result = select_windows(&pool, 2, 1);
        assert_eq!(result.len(), 2);
        assert_eq!((result[0].0.hour, result[0].0.minute), (0, 30));
        assert_eq!((result[1].0.hour, result[1].0.minute), (2, 0));
    }

    #[test]
    fn no_last_fire_makes_every_slot_eligible() {
        // A schedule that has never fired has no interval floor, so selection starts at index 0.
        let tz = chrono_tz::UTC;
        let pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        assert_eq!(earliest_eligible_index(&pool, None, 86_400, &tz), 0);
    }

    #[test]
    fn interval_guard_excludes_slots_within_one_interval() {
        // Last fire at the 00:00 slot with a 1-hour interval: the 00:00 and 00:30 slots fall
        // inside the interval and are excluded, so the first eligible slot is 01:00 (index 2).
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 5.0, 1.0),
            slot(0, 30, 5.0, 1.0),
            slot(1, 0, 5.0, 1.0),
            slot(1, 30, 5.0, 1.0),
        ];
        let last_fire = slot_epoch(&pool[0], &tz);
        assert_eq!(
            earliest_eligible_index(&pool, Some(last_fire), 3_600, &tz),
            2
        );
    }

    #[test]
    fn horizon_skip_reason_counts_time_to_next_run() {
        // 7d interval, last fired at now: the next run is a week out, beyond the price horizon.
        assert_eq!(
            horizon_skip_reason(Some(0), 7 * 86_400, 0),
            "next run in 7days, outside the price horizon"
        );
    }

    #[test]
    fn horizon_skip_reason_partial_day_remaining() {
        // humantime breaks the remaining duration down into its components.
        assert_eq!(
            horizon_skip_reason(Some(0), 86_400 + 3_600, 0),
            "next run in 1day 1h, outside the price horizon"
        );
    }

    #[test]
    fn horizon_skip_reason_without_last_fire() {
        // No recorded run means no countdown to report.
        assert_eq!(
            horizon_skip_reason(None, 86_400, 0),
            "no eligible price slot in the price horizon"
        );
    }

    #[test]
    fn interval_guard_returns_len_when_nothing_eligible() {
        // The interval floor lands past every slot, so no slot qualifies and the schedule is
        // left untouched (caller treats len() as "skip").
        let tz = chrono_tz::UTC;
        let pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        let last_fire = slot_epoch(&pool[1], &tz);
        assert_eq!(
            earliest_eligible_index(&pool, Some(last_fire), 3_600, &tz),
            pool.len()
        );
    }
}
