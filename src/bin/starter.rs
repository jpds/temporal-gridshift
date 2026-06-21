use iana_time_zone::get_timezone;
use temporal_gridshift::{SchedulerInput, workflow::SchedulerWorkflow};
use temporalio_client::{
    Client, ClientOptions, Connection,
    envconfig::LoadClientConfigProfileOptions,
    schedules::{
        CreateScheduleOptions, ScheduleAction, ScheduleCalendarSpec, ScheduleError, ScheduleSpec,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let task_queue =
        std::env::var("GRIDSHIFT_TASK_QUEUE").unwrap_or_else(|_| "gridshift".to_owned());
    let schedule_id =
        std::env::var("GRIDSHIFT_SCHEDULE_ID").unwrap_or_else(|_| "gridshift".to_owned());
    let workflow_id_prefix =
        std::env::var("GRIDSHIFT_WORKFLOW_ID").unwrap_or_else(|_| task_queue.clone());

    let query = std::env::var("GRIDSHIFT_QUERY")
        .map_err(|_| anyhow::anyhow!("GRIDSHIFT_QUERY is required"))?;

    // load_from_config reads the standard Temporal env vars (TEMPORAL_ADDRESS, TEMPORAL_NAMESPACE,
    // TEMPORAL_API_KEY, TEMPORAL_TLS*, ...). When TEMPORAL_API_KEY is set the SDK injects an
    // `authorization: Bearer` header on every RPC and enables TLS automatically.
    let (conn_opts, client_opts) =
        ClientOptions::load_from_config(LoadClientConfigProfileOptions::default())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    let connection = Connection::connect(conn_opts).await?;
    let client = Client::new(connection, client_opts)?;

    let timezone = std::env::var("GRIDSHIFT_TIMEZONE")
        .or_else(|_| get_timezone())
        .map_err(|e| anyhow::anyhow!("cannot determine timezone: {e}; set GRIDSHIFT_TIMEZONE"))?;

    timezone.parse::<chrono_tz::Tz>().map_err(|_| {
        anyhow::anyhow!(
            "invalid IANA timezone {timezone:?}; check GRIDSHIFT_TIMEZONE or system timezone"
        )
    })?;

    let input = SchedulerInput {
        query: query.clone(),
        timezone: timezone.clone(),
    };

    let action = ScheduleAction::start_workflow(
        SchedulerWorkflow::run,
        input,
        &task_queue,
        &workflow_id_prefix,
    );

    // Default to 20:00 with a 30-minute jitter; most providers publish next-day prices
    // by then. Override via GRIDSHIFT_SCHEDULE_HOUR for providers with different schedules.
    let schedule_hour: u8 = std::env::var("GRIDSHIFT_SCHEDULE_HOUR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let spec = ScheduleSpec {
        calendars: vec![
            ScheduleCalendarSpec::builder()
                .hour(schedule_hour.to_string())
                .build(),
        ],
        timezone_name: timezone.clone(),
        jitter: Some(std::time::Duration::from_secs(30 * 60)),
        ..Default::default()
    };

    match client
        .create_schedule(
            &schedule_id,
            CreateScheduleOptions::builder()
                .action(action)
                .spec(spec)
                .build(),
        )
        .await
    {
        Ok(handle) => {
            println!("Schedule created: {}", handle.schedule_id());
        }
        Err(ScheduleError::Rpc(s)) if s.code() == tonic::Code::AlreadyExists => {
            println!("Schedule {schedule_id} already exists.");
            println!("To recreate: temporal schedule delete --schedule-id {schedule_id} && rerun");
        }
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    }

    println!("  Task queue : {task_queue}");
    println!("  Timezone   : {timezone}");
    println!("  Query      : {query}");

    Ok(())
}
