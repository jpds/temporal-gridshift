use std::collections::HashMap;

use chrono::TimeZone;
use chrono_tz::Tz;

use good_lp::constraint::eq;
use good_lp::{Expression, ProblemVariables, Solution, SolverModel, Variable, variable};

use crate::{
    ChosenWindow, PlanWindowsResult, PricedWindow, ScheduleAssignment, ScheduleInfo, ScheduleRef,
    SkippedSchedule,
};

/// Convert a slot's local wall-clock start to UTC epoch seconds. Deterministic because
/// chrono-tz's IANA database is compiled in, so it is safe to call from workflow code.
fn slot_epoch(w: &PricedWindow, tz: &Tz) -> i64 {
    let Some(ndt) = w.date.and_hms_opt(w.hour, w.minute, 0) else {
        return i64::MIN;
    };
    // A DST spring-forward gap has no valid local time. i64::MIN makes the interval guard read
    // the slot as infinitely early so it is never selected.
    tz.from_local_datetime(&ndt)
        .earliest()
        .map(|dt| dt.timestamp())
        .unwrap_or(i64::MIN)
}

/// Index of the first slot a schedule may fire in without firing again less than one interval
/// after its last run, or `priced.len()` when none qualify.
pub(crate) fn earliest_eligible_index(
    priced: &[PricedWindow],
    last_fire_secs: Option<i64>,
    interval_secs: u64,
    tz: &Tz,
) -> usize {
    let Some(last_fire) = last_fire_secs else {
        return 0;
    };
    // An interval schedule fires at epoch + n*interval + phase, so pulling the phase earlier can
    // move the next firing into the previous run's interval. Requiring last_fire + interval keeps
    // the candidate at or after the next legal firing. Exact for intervals of a day or longer; for
    // sub-day intervals the phase also aliases to earlier firings the same day, which this
    // slot-level guard does not model.
    let floor = last_fire + interval_secs as i64;

    // priced is chronological, so the eligible slots form a contiguous suffix.
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

/// Assign each schedule to a price window using an ILP solver (HiGHS via good_lp).
///
/// Decision variables:
///   x[j][t] in {0,1}: job j starts at slot t.
///   w[t][d] in {0,1}: the window at slot t extends at least d slots.
///
/// Objective: minimize sum_t sum_d  price[t+d-1] * w[t][d].
///
/// Constraints:
///   (1) sum_t x[j][t] = 1                        -- each job fires exactly once
///   (2) w[t][d] >= x[j][t]  for dur[j] >= d      -- window wide enough for each job
///   (3) w[t][d] <= sum_{j: dur>=d} x[j][t]       -- no phantom windows at negative prices
pub(crate) fn plan_assignments(
    schedules: Vec<ScheduleInfo>,
    schedule_durations: &HashMap<ScheduleRef, u32>,
    priced: Vec<PricedWindow>,
    tz: &Tz,
    now_secs: i64,
    slot_duration_mins: u32,
) -> PlanWindowsResult {
    let mut assignments: Vec<ScheduleAssignment> = Vec::new();
    let mut skipped: Vec<SkippedSchedule> = Vec::new();

    // Phase 1: classify schedules as eligible or skipped.
    struct Job {
        namespace: String,
        schedule_id: String,
        interval_secs: u64,
        min_start: usize,
        duration_slots: usize,
    }

    let mut jobs: Vec<Job> = Vec::new();
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
            if min_start + duration_slots > priced.len() {
                skipped.push(SkippedSchedule {
                    namespace: s.namespace.clone(),
                    schedule_id: s.schedule_id.clone(),
                    reason: "job duration extends past the price horizon".to_owned(),
                });
                continue;
            }
            jobs.push(Job {
                namespace: s.namespace.clone(),
                schedule_id: s.schedule_id.clone(),
                interval_secs: s.interval_secs,
                min_start,
                duration_slots,
            });
        }
    }

    if jobs.is_empty() {
        return PlanWindowsResult {
            assignments,
            skipped,
        };
    }

    let n_jobs = jobs.len();
    let n_slots = priced.len();
    let max_dur = jobs.iter().map(|j| j.duration_slots).max().unwrap_or(1);
    let prices: Vec<f64> = priced.iter().map(|w| w.price_p_per_kwh).collect();

    // Phase 2: build and solve the ILP.
    let mut vars = ProblemVariables::default();

    // x[j][t]: None where t is before min_start[j] or t+dur[j] overruns the pool.
    let x: Vec<Vec<Option<Variable>>> = (0..n_jobs)
        .map(|j| {
            (0..n_slots)
                .map(|t| {
                    if t >= jobs[j].min_start && t + jobs[j].duration_slots <= n_slots {
                        Some(vars.add(variable().binary()))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect();

    // w[t][d]: None where t+d overruns the pool (d is 1-indexed, stored at d-1).
    let w: Vec<Vec<Option<Variable>>> = (0..n_slots)
        .map(|t| {
            (1..=max_dur)
                .map(|d| {
                    if t + d <= n_slots {
                        Some(vars.add(variable().binary()))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect();

    // Each window is counted once no matter how many jobs share its start slot, so co-located
    // jobs run concurrently and only the longest one drives the cost.
    let objective: Expression = (0..n_slots)
        .flat_map(|t| {
            let prices = &prices;
            let w = &w;
            (1..=max_dur)
                .filter_map(move |d| w[t][d - 1].map(|wv| Expression::from(wv) * prices[t + d - 1]))
        })
        .sum();

    let mut model = vars.minimise(objective).using(good_lp::default_solver);

    // Constraint (1): each job assigned to exactly one slot.
    for x_j in &x {
        let sum: Expression = x_j.iter().copied().sum();
        model.add_constraint(eq(sum, 1));
    }

    // Constraint (2): window must extend far enough to cover each assigned job.
    for j in 0..n_jobs {
        for t in 0..n_slots {
            if let Some(xjt) = x[j][t] {
                for d in 1..=jobs[j].duration_slots {
                    if let Some(wtd) = w[t][d - 1] {
                        model.add_constraint(wtd >> xjt);
                    }
                }
            }
        }
    }

    // Constraint (3): suppress phantom windows at slots with no covering job.
    for t in 0..n_slots {
        for d in 1..=max_dur {
            if let Some(wtd) = w[t][d - 1] {
                let eligible: Expression = (0..n_jobs)
                    .filter(|&j| jobs[j].duration_slots >= d)
                    .map(|j| x[j][t])
                    .sum();
                model.add_constraint(wtd << eligible);
            }
        }
    }

    // Phase 3: solve and extract assignments.
    let solution = match model.solve() {
        Ok(s) => s,
        Err(e) => {
            for job in &jobs {
                skipped.push(SkippedSchedule {
                    namespace: job.namespace.clone(),
                    schedule_id: job.schedule_id.clone(),
                    reason: format!("ILP solver failed: {e}"),
                });
            }
            return PlanWindowsResult {
                assignments,
                skipped,
            };
        }
    };

    for (j, job) in jobs.iter().enumerate() {
        match (0..n_slots).find(|&t| x[j][t].is_some_and(|v| solution.value(v) > 0.5)) {
            Some(t) => assignments.push(ScheduleAssignment {
                schedule_ref: ScheduleRef {
                    namespace: job.namespace.clone(),
                    schedule_id: job.schedule_id.clone(),
                },
                window: ChosenWindow {
                    date: priced[t].date,
                    hour: priced[t].hour,
                    minute: priced[t].minute,
                },
                interval_secs: job.interval_secs,
            }),
            None => skipped.push(SkippedSchedule {
                namespace: job.namespace.clone(),
                schedule_id: job.schedule_id.clone(),
                reason: "ILP assigned no slot".to_owned(),
            }),
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

    fn slot(hour: u32, minute: u32, price: f64) -> PricedWindow {
        PricedWindow {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            hour,
            minute,
            price_p_per_kwh: price,
        }
    }

    #[test]
    fn no_last_fire_makes_every_slot_eligible() {
        let tz = chrono_tz::UTC;
        let pool = vec![slot(0, 0, 5.0), slot(0, 30, 5.0)];
        assert_eq!(earliest_eligible_index(&pool, None, 86_400, &tz), 0);
    }

    #[test]
    fn interval_guard_excludes_slots_within_one_interval() {
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 5.0),
            slot(0, 30, 5.0),
            slot(1, 0, 5.0),
            slot(1, 30, 5.0),
        ];
        let last_fire = slot_epoch(&pool[0], &tz);
        assert_eq!(
            earliest_eligible_index(&pool, Some(last_fire), 3_600, &tz),
            2
        );
    }

    #[test]
    fn interval_guard_returns_len_when_nothing_eligible() {
        let tz = chrono_tz::UTC;
        let pool = vec![slot(0, 0, 5.0), slot(0, 30, 5.0)];
        let last_fire = slot_epoch(&pool[1], &tz);
        assert_eq!(
            earliest_eligible_index(&pool, Some(last_fire), 3_600, &tz),
            pool.len()
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
            slot(0, 0, 30.0),
            slot(0, 30, 25.0),
            slot(2, 0, 5.0),
            slot(2, 30, 10.0),
            slot(3, 0, 20.0),
            slot(3, 30, 30.0),
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
            slot(0, 0, 5.0),
            slot(0, 30, 15.0),
            slot(1, 0, 10.0),
            slot(1, 30, 20.0),
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
        let pool = vec![slot(0, 0, 5.0), slot(0, 30, 10.0), slot(1, 0, 20.0)];
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
    fn short_jobs_co_locate_with_long_job_for_free() {
        // Pool: 4 slots at prices 5, 6, 7, 8 (monotone increasing).
        // long_c needs a 2-slot window starting at 00:00 (cost 5+6=11).
        // short_a and short_b each fit in 1 slot; placing them at 00:00 alongside long_c
        // adds nothing to the objective because long_c's window already covers that slot.
        // Splitting them to any other slot costs more (e.g. 11+7=18). The ILP co-locates
        // all three at 00:00 as the globally optimal solution.
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 5.0),
            slot(0, 30, 6.0),
            slot(1, 0, 7.0),
            slot(1, 30, 8.0),
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
        for a in &result.assignments {
            assert_eq!(
                (a.window.hour, a.window.minute),
                (0, 0),
                "schedule {} should co-locate at 00:00 (globally optimal)",
                a.schedule_ref.schedule_id
            );
        }
    }

    #[test]
    fn four_hour_job_lands_in_cheap_five_hour_window() {
        // 18-slot pool: 4 expensive slots, then 10 cheap slots (5 hours), then 4 moderate.
        // A 4-hour job needs 8 consecutive slots; any start in slots 4-6 (02:00-03:00)
        // fits entirely in the cheap block. HiGHS may pick any of those tied starts.
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 30.0),
            slot(0, 30, 30.0),
            slot(1, 0, 30.0),
            slot(1, 30, 30.0),
            // cheap 5-hour block (10 slots)
            slot(2, 0, 3.0),
            slot(2, 30, 3.0),
            slot(3, 0, 3.0),
            slot(3, 30, 3.0),
            slot(4, 0, 3.0),
            slot(4, 30, 3.0),
            slot(5, 0, 3.0),
            slot(5, 30, 3.0),
            slot(6, 0, 3.0),
            slot(6, 30, 3.0),
            // moderate tail
            slot(7, 0, 15.0),
            slot(7, 30, 15.0),
            slot(8, 0, 15.0),
            slot(8, 30, 15.0),
        ];

        let schedules = vec![sched("heavy", 86_400, None)];
        let mut durations = HashMap::new();
        durations.insert(sref("heavy"), 240u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 1);
        let (h, m) = (
            result.assignments[0].window.hour,
            result.assignments[0].window.minute,
        );
        // Valid starts: slots 4-6 (02:00, 02:30, 03:00) -- all 8 slots in the cheap block.
        // HiGHS breaks ties among equally-priced starts non-deterministically.
        assert!(
            matches!((h, m), (2, 0) | (2, 30) | (3, 0)),
            "4-hour job should start in the cheap 5-hour window, got ({h}, {m})",
        );
    }

    #[test]
    fn short_job_co_locates_with_long_job_in_cheap_window() {
        // A 15-min job alongside a 4-hour job: both fire at the same phase offset, so the
        // window only needs to cover the longer one. Both should land somewhere in the cheap
        // block. This verifies the short job does not claim a single slot separately and push
        // the long job to a worse block.
        let tz = chrono_tz::UTC;
        let pool = vec![
            slot(0, 0, 30.0),
            slot(0, 30, 30.0),
            slot(1, 0, 30.0),
            slot(1, 30, 30.0),
            slot(2, 0, 3.0),
            slot(2, 30, 3.0),
            slot(3, 0, 3.0),
            slot(3, 30, 3.0),
            slot(4, 0, 3.0),
            slot(4, 30, 3.0),
            slot(5, 0, 3.0),
            slot(5, 30, 3.0),
            slot(6, 0, 3.0),
            slot(6, 30, 3.0),
            slot(7, 0, 15.0),
            slot(7, 30, 15.0),
            slot(8, 0, 15.0),
            slot(8, 30, 15.0),
        ];
        let schedules = vec![sched("heavy", 86_400, None), sched("light", 86_400, None)];
        let mut durations = HashMap::new();
        durations.insert(sref("heavy"), 240u32);
        durations.insert(sref("light"), 15u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 2);
        let heavy_hm = result
            .assignments
            .iter()
            .find(|a| a.schedule_ref.schedule_id == "heavy")
            .map(|a| (a.window.hour, a.window.minute))
            .unwrap();
        let light_hm = result
            .assignments
            .iter()
            .find(|a| a.schedule_ref.schedule_id == "light")
            .map(|a| (a.window.hour, a.window.minute))
            .unwrap();
        assert_eq!(heavy_hm, light_hm, "heavy and light should co-locate");
        let (h, m) = heavy_hm;
        // Valid starts: slots 4-6 (02:00, 02:30, 03:00) -- all 8 slots in the cheap block.
        assert!(
            matches!((h, m), (2, 0) | (2, 30) | (3, 0)),
            "jobs should start in the cheap window, got ({h}, {m})",
        );
    }

    #[test]
    fn ineligible_schedule_is_skipped_not_assigned() {
        let tz = chrono_tz::UTC;
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let epoch_02 = tz
            .from_utc_datetime(&date.and_hms_opt(2, 0, 0).unwrap())
            .timestamp();

        let pool = vec![slot(0, 0, 5.0), slot(0, 30, 5.0), slot(1, 0, 5.0)];
        // 7-day interval, last fired 2h into 2024-01-01: next run is ~Jan 8, outside the pool.
        let schedules = vec![sched("a", 7 * 86_400, Some(epoch_02))];
        let durations = HashMap::new();

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert!(result.assignments.is_empty());
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].schedule_id, "a");
    }

    #[test]
    fn job_placed_at_start_that_captures_negative_price_slot() {
        // Pool: 10p, -5p, 8p. A 2-slot job starting at 00:00 costs 10+(-5)=5p;
        // starting at 00:30 costs -5+8=3p. The ILP picks 00:30.
        // Negative prices make the window more attractive, not less.
        let tz = chrono_tz::UTC;
        let pool = vec![slot(0, 0, 10.0), slot(0, 30, -5.0), slot(1, 0, 8.0)];
        let schedules = vec![sched("a", 3600, None)];
        let mut durations = HashMap::new();
        durations.insert(sref("a"), 60u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 1);
        assert_eq!(
            (
                result.assignments[0].window.hour,
                result.assignments[0].window.minute
            ),
            (0, 30),
            "2-slot job should start at 00:30 to capture the negative-price slot (cost 3 vs 5)"
        );
    }

    #[test]
    fn job_skipped_when_duration_overruns_price_horizon() {
        // Schedule last fired at 00:00 with a 1-hour interval: eligible from 01:00 (slot 2).
        // A 2-hour (4-slot) duration needs slots 2-5, but the pool only has 4 slots (0-3).
        // min_start(2) + duration_slots(4) = 6 > 4, so the schedule is skipped.
        let tz = chrono_tz::UTC;
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let epoch_00 = tz
            .from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .timestamp();

        let pool = vec![
            slot(0, 0, 5.0),
            slot(0, 30, 5.0),
            slot(1, 0, 5.0),
            slot(1, 30, 5.0),
        ];
        let schedules = vec![sched("a", 3600, Some(epoch_00))];
        let mut durations = HashMap::new();
        durations.insert(sref("a"), 120u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert!(result.assignments.is_empty());
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].schedule_id, "a");
        assert_eq!(
            result.skipped[0].reason,
            "job duration extends past the price horizon"
        );
    }

    #[test]
    fn jobs_with_incompatible_eligibility_land_at_separate_slots() {
        // Job A needs 2 slots; the pool has 2 slots, so A must start at slot 0.
        // Job B last fired at 00:00 with a 30-min interval, giving min_start=1 (00:30).
        // With only 2 slots, B can only start at slot 1. The ILP must place them separately.
        let tz = chrono_tz::UTC;
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let epoch_00 = tz
            .from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .timestamp();

        let pool = vec![slot(0, 0, 5.0), slot(0, 30, 3.0)];
        let schedules = vec![sched("a", 3600, None), sched("b", 1800, Some(epoch_00))];
        let mut durations = HashMap::new();
        durations.insert(sref("a"), 60u32);
        durations.insert(sref("b"), 30u32);

        let result = plan_assignments(schedules, &durations, pool, &tz, 0, 30);

        assert_eq!(result.assignments.len(), 2);
        let a = result
            .assignments
            .iter()
            .find(|x| x.schedule_ref.schedule_id == "a")
            .unwrap();
        let b = result
            .assignments
            .iter()
            .find(|x| x.schedule_ref.schedule_id == "b")
            .unwrap();
        assert_eq!((a.window.hour, a.window.minute), (0, 0));
        assert_eq!((b.window.hour, b.window.minute), (0, 30));
    }
}
