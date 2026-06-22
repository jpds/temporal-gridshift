use std::collections::HashMap;

use chrono::TimeZone;
use chrono_tz::Tz;

use crate::{
    ChosenWindow, PlanWindowsResult, PricedWindow, ScheduleAssignment, ScheduleInfo, ScheduleRef,
    SkippedSchedule,
};

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
pub(crate) fn earliest_eligible_index(
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
pub(crate) fn horizon_skip_reason(
    last_fire_secs: Option<i64>,
    interval_secs: u64,
    now_secs: i64,
) -> String {
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
/// Called with `fires_per_day = 1`, each schedule claims the single cheapest block in the
/// pool and `mark_claimed` then steers later schedules away from it. The multi-segment path
/// exists for the unit tests and any future caller that wants several non-overlapping blocks
/// from one pool.
pub(crate) fn select_windows(
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
            let last_start = seg_end.saturating_sub(duration).max(seg_start);

            let best = (seg_start..=last_start)
                .max_by(|&a, &b| {
                    let end_a = (a + duration).min(priced.len());
                    let end_b = (b + duration).min(priced.len());
                    let wa = weight_prefix[end_a] - weight_prefix[a];
                    let wb = weight_prefix[end_b] - weight_prefix[b];
                    wa.total_cmp(&wb)
                        .then_with(|| {
                            let price_a = price_prefix[end_a] - price_prefix[a];
                            let price_b = price_prefix[end_b] - price_prefix[b];
                            price_b.total_cmp(&price_a)
                        })
                        .then_with(|| b.cmp(&a))
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
/// `PricedWindow` skips weight on serde, so activities always receive zeroed weights and must
/// call this before `select_windows`.
pub(crate) fn normalize_weights(priced: &mut [PricedWindow]) {
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
pub(crate) fn mark_claimed(priced: &mut [PricedWindow], start: usize, duration_slots: usize) {
    let end = (start + duration_slots).min(priced.len());
    for w in &mut priced[start..end] {
        if duration_slots > 1 {
            w.weight = 0.0;
        } else {
            w.weight *= 0.5;
        }
    }
}

/// Assign each schedule to a price window.
///
/// Schedules in the same window fire at the same phase offset and run concurrently, so the
/// window only needs to cover the longest job. The algorithm processes eligible jobs
/// longest-first; each tries to join the existing group whose merged window (taking the
/// max duration and tightest min_start across all members) has per-slot weight at least
/// equal to the job's standalone best. If no group qualifies, the job starts its own.
///
/// Groups then land in the live pool, most-constrained first (highest max_min_start, then
/// longest duration). mark_claimed runs after each placement to downweight used slots for
/// subsequent groups.
pub(crate) fn plan_assignments(
    schedules: Vec<ScheduleInfo>,
    schedule_durations: &HashMap<ScheduleRef, u32>,
    mut priced: Vec<PricedWindow>,
    tz: &Tz,
    now_secs: i64,
    slot_duration_mins: u32,
) -> PlanWindowsResult {
    normalize_weights(&mut priced);

    let mut assignments: Vec<ScheduleAssignment> = Vec::new();
    let mut skipped: Vec<SkippedSchedule> = Vec::new();

    // Phase 1: classify schedules as eligible or skipped.
    struct EligibleJob {
        namespace: String,
        schedule_id: String,
        interval_secs: u64,
        min_start: usize,
        duration_slots: usize,
    }

    let mut eligible: Vec<EligibleJob> = Vec::new();
    for s in &schedules {
        let min_start = earliest_eligible_index(&priced, s.last_fire_secs, s.interval_secs, tz);
        if min_start >= priced.len() {
            skipped.push(SkippedSchedule {
                namespace: s.namespace.clone(),
                schedule_id: s.schedule_id.clone(),
                reason: horizon_skip_reason(s.last_fire_secs, s.interval_secs, now_secs),
            });
        } else {
            let sref = ScheduleRef {
                namespace: s.namespace.clone(),
                schedule_id: s.schedule_id.clone(),
            };
            let dur = schedule_durations
                .get(&sref)
                .copied()
                .unwrap_or(slot_duration_mins);
            let duration_slots = dur.div_ceil(slot_duration_mins).max(1) as usize;
            eligible.push(EligibleJob {
                namespace: s.namespace.clone(),
                schedule_id: s.schedule_id.clone(),
                interval_secs: s.interval_secs,
                min_start,
                duration_slots,
            });
        }
    }

    // Longest jobs first: they claim cheap multi-slot blocks before shorter jobs decide
    // whether to join. Secondary sort by schedule_id keeps replay order deterministic.
    eligible.sort_by(|a, b| {
        b.duration_slots
            .cmp(&a.duration_slots)
            .then_with(|| a.schedule_id.cmp(&b.schedule_id))
    });

    // Phase 2: greedy concurrent grouping, scored against a frozen weight snapshot.
    //
    // score_window returns (weight_per_slot, absolute_start_index) for the best window of
    // `dur_slots` consecutive slots starting at or after `min_start`. Returns None if the
    // suffix from min_start is empty.
    let snapshot = priced.clone();
    let score_window = |min_start: usize, dur_slots: usize| -> Option<(f64, usize)> {
        let sub = &snapshot[min_start..];
        if sub.is_empty() {
            return None;
        }
        select_windows(sub, 1, dur_slots)
            .into_iter()
            .next()
            .map(|(_, rel_start)| {
                let abs = min_start + rel_start;
                let end = (abs + dur_slots).min(snapshot.len());
                let wsum: f64 = snapshot[abs..end].iter().map(|w| w.weight).sum();
                (wsum / dur_slots.max(1) as f64, abs)
            })
    };

    struct Group {
        member_indices: Vec<usize>,
        max_min_start: usize,
        max_duration_slots: usize,
    }

    let mut groups: Vec<Group> = Vec::new();

    for (job_idx, job) in eligible.iter().enumerate() {
        let standalone_wps =
            score_window(job.min_start, job.duration_slots).map_or(0.0, |(wps, _)| wps);

        // Find the group whose merged window scores highest while matching or beating this
        // job's standalone per-slot weight. The >= (with epsilon) threshold lets a neutral
        // merge join: same cheapness, one fewer window.
        let mut best: Option<(usize, f64)> = None;
        for (gi, group) in groups.iter().enumerate() {
            let new_ms = group.max_min_start.max(job.min_start);
            let new_dur = group.max_duration_slots.max(job.duration_slots);
            if let Some((wps, _)) = score_window(new_ms, new_dur)
                && wps >= standalone_wps - f64::EPSILON
                && best.is_none_or(|(_, best_wps)| wps > best_wps)
            {
                best = Some((gi, wps));
            }
        }

        match best {
            Some((gi, _)) => {
                let g = &mut groups[gi];
                g.member_indices.push(job_idx);
                g.max_min_start = g.max_min_start.max(job.min_start);
                g.max_duration_slots = g.max_duration_slots.max(job.duration_slots);
            }
            None => {
                groups.push(Group {
                    member_indices: vec![job_idx],
                    max_min_start: job.min_start,
                    max_duration_slots: job.duration_slots,
                });
            }
        }
    }

    // Phase 3: place groups into the live pool, most-constrained first so high-floor
    // groups take their slots before later groups can downweight them.
    groups.sort_by(|a, b| {
        b.max_min_start
            .cmp(&a.max_min_start)
            .then_with(|| b.max_duration_slots.cmp(&a.max_duration_slots))
    });

    for group in groups {
        let selected = select_windows(&priced[group.max_min_start..], 1, group.max_duration_slots);
        match selected.into_iter().next() {
            Some((window, rel_start)) => {
                let abs_start = group.max_min_start + rel_start;
                mark_claimed(&mut priced, abs_start, group.max_duration_slots);
                for job_idx in &group.member_indices {
                    let job = &eligible[*job_idx];
                    assignments.push(ScheduleAssignment {
                        schedule_ref: ScheduleRef {
                            namespace: job.namespace.clone(),
                            schedule_id: job.schedule_id.clone(),
                        },
                        window: window.clone(),
                        interval_secs: job.interval_secs,
                    });
                }
            }
            None => {
                for job_idx in &group.member_indices {
                    let job = &eligible[*job_idx];
                    skipped.push(SkippedSchedule {
                        namespace: job.namespace.clone(),
                        schedule_id: job.schedule_id.clone(),
                        reason: "no price slot available for group".to_owned(),
                    });
                }
            }
        }
    }

    PlanWindowsResult {
        assignments,
        skipped,
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
        let pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        let result = select_windows(&pool, 5, 1);
        assert_eq!(hms(&result), vec![(0, 0, 0), (0, 30, 1)]);
    }

    #[test]
    fn last_segment_absorbs_remainder_slots() {
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
        let mut pool = vec![slot(0, 0, -5.0, 0.0), slot(0, 30, 15.0, 0.0)];
        normalize_weights(&mut pool);
        assert_eq!(pool[0].weight, 1.0);
        assert_eq!(pool[1].weight, 0.0);
    }

    #[test]
    fn normalize_weights_flat_prices_all_one() {
        let mut pool = vec![slot(0, 0, 12.0, 0.0), slot(0, 30, 12.0, 0.0)];
        normalize_weights(&mut pool);
        assert!(pool.iter().all(|w| w.weight == 1.0));
    }

    #[test]
    fn mark_claimed_zeroes_multi_slot_block() {
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
        let mut pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 0.8)];
        mark_claimed(&mut pool, 0, 1);
        assert_eq!(pool[0].weight, 0.5);
        assert_eq!(pool[1].weight, 0.8);
    }

    #[test]
    fn mark_claimed_clamps_block_to_pool_end() {
        let mut pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        mark_claimed(&mut pool, 1, 5);
        assert_eq!(pool[1].weight, 0.0);
    }

    #[test]
    fn block_does_not_cross_segment_boundary() {
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
        let tz = chrono_tz::UTC;
        let pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        assert_eq!(earliest_eligible_index(&pool, None, 86_400, &tz), 0);
    }

    #[test]
    fn interval_guard_excludes_slots_within_one_interval() {
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
        assert_eq!(
            horizon_skip_reason(Some(0), 7 * 86_400, 0),
            "next run in 7days, outside the price horizon"
        );
    }

    #[test]
    fn horizon_skip_reason_partial_day_remaining() {
        assert_eq!(
            horizon_skip_reason(Some(0), 86_400 + 3_600, 0),
            "next run in 1day 1h, outside the price horizon"
        );
    }

    #[test]
    fn horizon_skip_reason_without_last_fire() {
        assert_eq!(
            horizon_skip_reason(None, 86_400, 0),
            "no eligible price slot in the price horizon"
        );
    }

    #[test]
    fn interval_guard_returns_len_when_nothing_eligible() {
        let tz = chrono_tz::UTC;
        let pool = vec![slot(0, 0, 5.0, 1.0), slot(0, 30, 5.0, 1.0)];
        let last_fire = slot_epoch(&pool[1], &tz);
        assert_eq!(
            earliest_eligible_index(&pool, Some(last_fire), 3_600, &tz),
            pool.len()
        );
    }

    // plan_assignments tests

    fn sched(id: &str, interval_secs: u64, last_fire_secs: Option<i64>) -> ScheduleInfo {
        ScheduleInfo {
            namespace: "ns".to_string(),
            schedule_id: id.to_string(),
            interval_secs,
            last_fire_secs,
        }
    }

    fn sref(id: &str) -> ScheduleRef {
        ScheduleRef {
            namespace: "ns".to_string(),
            schedule_id: id.to_string(),
        }
    }

    #[test]
    fn pack_four_short_schedules_into_cheapest_slot() {
        // 2x10m + 1x5m + 1x2m = 27 minutes; all fit in one 30-minute slot.
        // The cheapest slot is at 02:00; all four schedules should land there.
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 30.0, 0.0),
            slot(0, 30, 25.0, 0.0),
            slot(2, 0, 5.0, 0.0),
            slot(2, 30, 10.0, 0.0),
            slot(3, 0, 20.0, 0.0),
            slot(3, 30, 30.0, 0.0),
        ];
        let schedules = vec![
            sched("a", 3600, None),
            sched("b", 3600, None),
            sched("c", 3600, None),
            sched("d", 3600, None),
        ];
        let mut durations = HashMap::new();
        durations.insert(sref("a"), 10u32);
        durations.insert(sref("b"), 10u32);
        durations.insert(sref("c"), 5u32);
        durations.insert(sref("d"), 2u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 4);
        assert!(result.skipped.is_empty());
        for a in &result.assignments {
            assert_eq!(
                (a.window.hour, a.window.minute),
                (2, 0),
                "schedule {} should be packed at 02:00",
                a.schedule_ref.schedule_id
            );
        }
    }

    #[test]
    fn pack_respects_max_min_start_across_schedules() {
        // Schedule 'b' last fired at 00:00 with a 1-hour interval, so it cannot fire before
        // 01:00. Pack should use the cheapest slot at or after 01:00 for both.
        let tz = chrono_tz::UTC;
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let epoch_00 = tz
            .from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .timestamp();

        let pool = vec![
            slot(0, 0, 5.0, 0.0),
            slot(0, 30, 15.0, 0.0),
            slot(1, 0, 10.0, 0.0),
            slot(1, 30, 20.0, 0.0),
        ];
        let schedules = vec![sched("a", 3600, None), sched("b", 3600, Some(epoch_00))];
        let mut durations = HashMap::new();
        durations.insert(sref("a"), 10u32);
        durations.insert(sref("b"), 10u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 2);
        for a in &result.assignments {
            assert_eq!(
                (a.window.hour, a.window.minute),
                (1, 0),
                "schedule {} should be at 01:00 (cheapest slot where both are eligible)",
                a.schedule_ref.schedule_id
            );
        }
    }

    #[test]
    fn concurrent_jobs_share_cheapest_window() {
        // Two 20-minute jobs run concurrently: the window only needs to cover 20 minutes, so
        // both fit in the cheapest single slot. They should co-locate there.
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 5.0, 0.0),
            slot(0, 30, 10.0, 0.0),
            slot(1, 0, 20.0, 0.0),
        ];
        let schedules = vec![sched("a", 3600, None), sched("b", 3600, None)];
        let mut durations = HashMap::new();
        durations.insert(sref("a"), 20u32);
        durations.insert(sref("b"), 20u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 2);
        for a in &result.assignments {
            assert_eq!(
                (a.window.hour, a.window.minute),
                (0, 0),
                "schedule {} should co-locate at the cheapest slot",
                a.schedule_ref.schedule_id
            );
        }
    }

    #[test]
    fn long_job_gets_cheapest_block_short_jobs_follow() {
        // The 40-minute job needs a 2-slot window and claims the cheapest block at 00:00.
        // The two 10-minute jobs stay separate: joining would lower their per-slot quality
        // (2-slot wps 0.833 vs standalone 1-slot wps 1.0), so they form their own group.
        // After 00:00-00:30 are claimed, they land in the next cheapest slot.
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 5.0, 0.0),
            slot(0, 30, 6.0, 0.0),
            slot(1, 0, 7.0, 0.0),
            slot(1, 30, 8.0, 0.0),
        ];
        let schedules = vec![
            sched("short_a", 3600, None),
            sched("short_b", 3600, None),
            sched("long_c", 3600, None),
        ];
        let mut durations = HashMap::new();
        durations.insert(sref("short_a"), 10u32);
        durations.insert(sref("short_b"), 10u32);
        durations.insert(sref("long_c"), 40u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 3);
        let long_c = result
            .assignments
            .iter()
            .find(|a| a.schedule_ref.schedule_id == "long_c")
            .unwrap();
        assert_eq!(
            (long_c.window.hour, long_c.window.minute),
            (0, 0),
            "long_c should claim the cheapest 2-slot block"
        );
        for a in result
            .assignments
            .iter()
            .filter(|a| a.schedule_ref.schedule_id != "long_c")
        {
            assert_ne!(
                (a.window.hour, a.window.minute),
                (0, 0),
                "short job {} should not overlap with long_c's claimed block",
                a.schedule_ref.schedule_id
            );
        }
    }

    #[test]
    fn four_hour_job_lands_in_cheap_five_hour_window() {
        // 18-slot pool: 4 expensive slots, then 10 cheap slots (5 hours), then 4 moderate.
        // A 4-hour job needs 8 consecutive slots; the cheapest 8-slot block sits inside the
        // cheap run at 02:00.
        let tz = chrono_tz::UTC;
        let mut pool = vec![
            slot(0, 0, 30.0, 0.0),
            slot(0, 30, 30.0, 0.0),
            slot(1, 0, 30.0, 0.0),
            slot(1, 30, 30.0, 0.0),
            // cheap 5-hour block (10 slots)
            slot(2, 0, 3.0, 0.0),
            slot(2, 30, 3.0, 0.0),
            slot(3, 0, 3.0, 0.0),
            slot(3, 30, 3.0, 0.0),
            slot(4, 0, 3.0, 0.0),
            slot(4, 30, 3.0, 0.0),
            slot(5, 0, 3.0, 0.0),
            slot(5, 30, 3.0, 0.0),
            slot(6, 0, 3.0, 0.0),
            slot(6, 30, 3.0, 0.0),
            // moderate tail
            slot(7, 0, 15.0, 0.0),
            slot(7, 30, 15.0, 0.0),
            slot(8, 0, 15.0, 0.0),
            slot(8, 30, 15.0, 0.0),
        ];
        normalize_weights(&mut pool);

        // Best 8-slot block: 02:00-05:30 (all 3p, weight 1.0 each).
        let result = select_windows(&pool, 1, 8);
        assert_eq!(result.len(), 1);
        assert_eq!((result[0].0.hour, result[0].0.minute), (2, 0));

        // Same result through plan_assignments with a single 4-hour (240 min) schedule.
        let schedules = vec![sched("heavy", 86_400, None)];
        let mut durations = HashMap::new();
        durations.insert(sref("heavy"), 240u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 1);
        assert_eq!(
            (
                result.assignments[0].window.hour,
                result.assignments[0].window.minute
            ),
            (2, 0),
            "4-hour job should start at the beginning of the cheap 5-hour window"
        );
    }

    #[test]
    fn short_job_co_locates_with_long_job_in_cheap_window() {
        // A 15-min job alongside a 4-hour job: both fire at the same phase offset, so the
        // window only needs to cover the longer one. Both should land at 02:00 (start of
        // the cheap 5-hour block). This verifies the short job does not claim a single slot
        // separately and push the long job to a worse block.
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 30.0, 0.0),
            slot(0, 30, 30.0, 0.0),
            slot(1, 0, 30.0, 0.0),
            slot(1, 30, 30.0, 0.0),
            slot(2, 0, 3.0, 0.0),
            slot(2, 30, 3.0, 0.0),
            slot(3, 0, 3.0, 0.0),
            slot(3, 30, 3.0, 0.0),
            slot(4, 0, 3.0, 0.0),
            slot(4, 30, 3.0, 0.0),
            slot(5, 0, 3.0, 0.0),
            slot(5, 30, 3.0, 0.0),
            slot(6, 0, 3.0, 0.0),
            slot(6, 30, 3.0, 0.0),
            slot(7, 0, 15.0, 0.0),
            slot(7, 30, 15.0, 0.0),
            slot(8, 0, 15.0, 0.0),
            slot(8, 30, 15.0, 0.0),
        ];
        let schedules = vec![sched("heavy", 86_400, None), sched("light", 86_400, None)];
        let mut durations = HashMap::new();
        durations.insert(sref("heavy"), 240u32);
        durations.insert(sref("light"), 15u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 2);
        for a in &result.assignments {
            assert_eq!(
                (a.window.hour, a.window.minute),
                (2, 0),
                "schedule {} should co-locate at 02:00 with the long job",
                a.schedule_ref.schedule_id
            );
        }
    }

    #[test]
    fn ineligible_schedule_is_skipped_not_assigned() {
        let tz = chrono_tz::UTC;
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let epoch_02 = tz
            .from_utc_datetime(&date.and_hms_opt(2, 0, 0).unwrap())
            .timestamp();

        let pool = vec![
            slot(0, 0, 5.0, 0.0),
            slot(0, 30, 5.0, 0.0),
            slot(1, 0, 5.0, 0.0),
        ];
        // 7-day interval, last fired 2h into 2024-01-01: next run is ~Jan 8, outside the pool.
        let schedules = vec![sched("a", 7 * 86_400, Some(epoch_02))];
        let durations = HashMap::new();

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert!(result.assignments.is_empty());
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].schedule_id, "a");
    }
}
