use std::sync::Arc;

use temporal_gridshift::{
    PriceProvider, activities::GridshiftActivities, providers::octopus::OctopusProvider,
    workflow::SchedulerWorkflow,
};
use temporalio_client::{
    Client, ClientOptions, Connection, envconfig::LoadClientConfigProfileOptions,
};
use temporalio_common::telemetry::{
    Logger, TelemetryOptions, construct_filter_string, telemetry_init_global,
};
use temporalio_sdk::{Worker, WorkerOptions};
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let task_queue =
        std::env::var("GRIDSHIFT_TASK_QUEUE").unwrap_or_else(|_| "gridshift".to_owned());

    let provider_name = std::env::var("GRIDSHIFT_PROVIDER")
        .map_err(|_| anyhow::anyhow!("GRIDSHIFT_PROVIDER is required"))?;
    let provider: Arc<dyn PriceProvider> = match provider_name.as_str() {
        "octopus" => Arc::new(OctopusProvider::new()),
        p => return Err(anyhow::anyhow!("unknown provider: {p:?}")),
    };

    // new_assume_tokio installs the SDK's log subscriber on the current thread only, but
    // activities run on arbitrary tokio worker threads; install a global subscriber so
    // their warnings reach the console.
    telemetry_init_global(
        TelemetryOptions::builder()
            .logging(Logger::Console {
                filter: construct_filter_string(tracing::Level::WARN, tracing::Level::INFO),
            })
            .build(),
    )
    .map_err(|e| anyhow::anyhow!("telemetry init: {e}"))?;

    let runtime = CoreRuntime::new_assume_tokio(
        RuntimeOptions::builder()
            .telemetry_options(TelemetryOptions::builder().build())
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    )?;

    // load_from_config reads the standard Temporal env vars (TEMPORAL_ADDRESS, TEMPORAL_NAMESPACE,
    // TEMPORAL_API_KEY, TEMPORAL_TLS*, ...). When TEMPORAL_API_KEY is set the SDK injects an
    // `authorization: Bearer` header on every RPC and enables TLS automatically.
    let (conn_opts, client_opts) =
        ClientOptions::load_from_config(LoadClientConfigProfileOptions::default())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    let connection = Connection::connect(conn_opts).await?;
    let client = Client::new(connection.clone(), client_opts.clone())?;

    let worker_options = WorkerOptions::new(&task_queue)
        .register_workflow::<SchedulerWorkflow>()
        .map_err(|e| anyhow::anyhow!("workflow registration failed: {e}"))?
        .register_activities(GridshiftActivities {
            connection: Arc::new(connection),
            client_opts,
            provider,
        })
        .build();

    let mut worker =
        Worker::new(&runtime, client, worker_options).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("Worker running | task queue: {task_queue}");
    worker.run().await?;
    Ok(())
}
