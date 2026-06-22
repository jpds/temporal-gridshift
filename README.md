# Temporal Gridshift

⏳️⚡️🕐

Gridshift is a [Temporal](https://temporal.io) integration which takes workflow schedules configured
with intervals and offsets the scheduled times to the cheapest electricity windows using data from
a provider's API.

## Supported providers

 * [Octopus Energy](https://octopus.energy/) - [Agile tariff](https://octopus.energy/agile/).

## How it works

1. The `SchedulerWorkflow` runs on a Temporal schedule (daily by default).
2. It calls `ListNamespaces` to enumerate every namespace on the cluster (excluding
   `temporal-system`), then runs the visibility query (e.g. `EnergyIntensive = true`) in each
   one. Namespaces where the query fails (search attribute not registered, permission denied, etc.)
   are skipped gracefully; a reason is recorded in the workflow result so it is visible in the
   Temporal Web UI.
3. It fetches half-hourly Agile prices from the Octopus API for today and tomorrow, then combines
   the still-future slots into one pool and re-normalizes each slot's cheapness weight across it.
4. It groups schedules that can share a firing window and assigns each group to the cheapest
   available block. Schedules in the same group fire at the same phase offset and run
   concurrently, so the window only needs to cover the longest job's duration. Grouping works
   longest-first: a shorter job joins an existing group when the merged window (widened to the
   longest member's duration and constrained to the latest member's eligibility floor) is at
   least as cheap per slot as the job would get on its own. If no group qualifies, the job
   starts its own. Once a group claims a block, those slots are down-weighted so subsequent
   groups spread out rather than stacking on the same window.
5. To avoid running a job more often than its interval, slots less than one interval after the
   schedule's last actual run are excluded; if that leaves no eligible slot, the schedule is left
   on its current spec.
6. Each schedule's interval phase is rewritten so its firing lands in the chosen window.

## Scheduling behavior

### Never fires a job sooner than its interval

An interval schedule fires at `epoch + n*interval + phase`, so shifting the phase earlier could
pull the next firing forward and run the job twice within one period. Gridshift prevents this by
excluding any slot less than one interval after the schedule's last actual run; if that leaves no
eligible slot, the schedule is left on its current spec.

For example, a 24h job that ran cheaply at 02:00 won't be re-pointed to a cheaper 15:00 slot later
the same day, since 15:00 is only 13 hours after the last run.

### Intervals longer than the price horizon

Octopus only publishes prices about a day ahead, so a schedule with a longer interval (e.g.
`--interval 7d`) can't be optimized until its next run is near. Until then, every candidate slot
falls within one interval of the last run, so the schedule is reported as skipped, with a reason
like `next run in 5days, outside the price horizon`. That's expected, not an error.

Consequences:

- On the run day, Gridshift can only move the firing later than one interval after the last run,
  never earlier: a weekly job last run at 14:00 can't be pulled into the cheap small hours of its
  next run day, since that would shorten the gap below one interval.
- A schedule that has never run uses its first `futureActionTimes` entry instead of a last-run
  time: if that first scheduled time falls within the price window, Gridshift places it in the
  cheapest slot; otherwise it's skipped the same way, so a newly created long-interval schedule
  doesn't land in a cheap slot today when its first run was meant to be weeks out.

## Setup

### 1. Register the search attribute

Gridshift discovers managed schedules via a custom Temporal search attribute. Create it in each
namespace whose schedules you want Gridshift to manage:

```sh
temporal operator search-attribute create \
  --namespace my-namespace \
  --name EnergyIntensive \
  --type Bool
```

Namespaces that do not have the attribute registered are skipped automatically; no schedules in
those namespaces will be touched.

### 2. Tag your schedules

When creating a schedule you want Gridshift to manage, add the search attribute:

```sh
temporal schedule create \
  --schedule-id my-job \
  --workflow-id my-job-wf \
  --type MyWorkflow \
  --task-queue my-queue \
  --interval 24h \
  --schedule-search-attribute 'EnergyIntensive=true'
```

Any Temporal visibility query can be used; `EnergyIntensive = true` is the convention, but you can
filter by any attribute via `GRIDSHIFT_QUERY`.

### 3. Run the worker

It is recommended to run Gridshift in its own dedicated namespace so its schedule and workflows
are clearly separated from the workloads it manages:

```sh
temporal operator namespace create --namespace gridshift
```

```sh
export GRIDSHIFT_PROVIDER=octopus
export OCTOPUS_API_KEY=your-api-key
export GRIDSHIFT_QUERY='EnergyIntensive = true'
export TEMPORAL_NAMESPACE=gridshift

./worker
```

The worker connects to Temporal using the standard Temporal CLI environment variables
(`TEMPORAL_ADDRESS`, `TEMPORAL_NAMESPACE`) or a local config profile. The configured namespace
is used for the worker's own task queue and schedule; managed schedules are discovered across all
namespaces on the cluster regardless of this setting.

### 4. Create the Gridshift schedule

Run once to register the daily trigger:

```sh
export GRIDSHIFT_QUERY='EnergyIntensive = true'

./starter
```

This creates a Temporal schedule named `gridshift` that fires the `SchedulerWorkflow` at 20:00
local time each day (with a 30-minute jitter), after Octopus has published next-day rates.

## Configuration

| Variable | Default | Description |
|---|---|---|
| `GRIDSHIFT_PROVIDER` | required | Price provider to use (supported: `octopus`) |
| `OCTOPUS_API_KEY` | required | Octopus Energy API key |
| `GRIDSHIFT_QUERY` | required | Temporal visibility query for managed schedules |
| `GRIDSHIFT_TIMEZONE` | system timezone | IANA timezone name (e.g. `Europe/London`) |
| `GRIDSHIFT_TASK_QUEUE` | `gridshift` | Temporal task queue |
| `GRIDSHIFT_SCHEDULE_ID` | `gridshift` | ID of the gridshift Temporal schedule |
| `GRIDSHIFT_SCHEDULE_HOUR` | `20` | Hour the scheduler runs each day (30-minute jitter applied) |
| `TEMPORAL_ADDRESS` | `localhost:7233` | Temporal frontend address |
| `TEMPORAL_NAMESPACE` | `default` | Namespace for the worker's task queue and own schedule |
| `TEMPORAL_API_KEY` | _(none)_ | API key for Temporal Cloud / JWT auth |

## Building

The recommended build uses the Nix flake, which pins all dependencies including the Temporal Rust
SDK:

```sh
nix build
```

Binaries are placed at `result/bin/worker` and `result/bin/starter`.

## NixOS integration test

```sh
nix flake check -L
```

The test spins up three VMs:

* An Octopus API mock
* A Temporal server
* The Gridshift worker

The scheduler is triggered, and verifies that managed schedule specs are updated to fire within the
cheap window defined by the mock price data. A second namespace (`restricted`) is created without
the `EnergyIntensive` attribute registered; the test asserts that Gridshift skips it and leaves
its schedule untouched.

## License

MIT

---

This project is not affiliated with or endorsed by Temporal Technologies Inc.
