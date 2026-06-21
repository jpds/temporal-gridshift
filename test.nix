{ gridshift }:
{ lib, pkgs, ... }:
let
  # Pre-generate a CA and server cert for api.octopus.energy at eval time so
  # the CA can be added to security.pki.certificates (a build-time option).
  testCerts = pkgs.runCommand "gridshift-test-certs" { buildInputs = [ pkgs.openssl ]; } ''
    mkdir -p $out
    openssl genrsa -out $out/ca.key 2048
    openssl req -new -x509 -days 3650 -key $out/ca.key \
      -subj "/CN=Gridshift Test CA" -out $out/ca.crt

    openssl genrsa -out $out/server.key 2048
    openssl req -new -key $out/server.key \
      -subj "/CN=api.octopus.energy" -out $out/server.csr
    printf 'subjectAltName=DNS:api.octopus.energy\n' > $out/ext.cnf
    openssl x509 -req -days 3650 \
      -in $out/server.csr -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
      -extfile $out/ext.cnf -out $out/server.crt
  '';
in
{
  name = "gridshift";

  nodes = {
    api =
      { pkgs, ... }:
      {
        networking.firewall.allowedTCPPorts = [ 443 ];

        services.caddy = {
          enable = true;
          virtualHosts."https://api.octopus.energy" = {
            extraConfig = ''
              tls ${testCerts}/server.crt ${testCerts}/server.key

              handle /v1/graphql/ {
                respond `{"data":{"obtainKrakenToken":{"token":"test-token"},"viewer":{"accounts":[{"number":"TEST-ACCOUNT"}]}}}` 200
              }

              handle /v1/accounts/TEST-ACCOUNT/ {
                respond `{"number":"TEST-ACCOUNT","properties":[{"id":1,"moved_in_at":"2020-01-01T00:00:00+00:00","moved_out_at":null,"address_line_1":"1 Test St","address_line_2":"","address_line_3":"","town":"Testville","county":"","postcode":"TE1 1ST","electricity_meter_points":[{"mpan":"1234567890","profile_class":1,"consumption_standard":3000,"meters":[{"serial_number":"TEST01","registers":[]}],"agreements":[{"tariff_code":"E-1R-AGILE-FLEX-22-11-25-C","valid_from":"2020-01-01T00:00:00+00:00","valid_to":null}],"is_export":false}],"gas_meter_points":[]}]}` 200
              }

              # Rates: 48 half-hourly slots; 02:00-04:30 UTC are cheap (5 p/kWh)
              handle /v1/products/*/electricity-tariffs/*/standard-unit-rates/ {
                respond `{"results":[{"value_inc_vat":30,"valid_from":"2024-01-01T00:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T00:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T01:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T01:30:00Z"},{"value_inc_vat":5,"valid_from":"2024-01-01T02:00:00Z"},{"value_inc_vat":5,"valid_from":"2024-01-01T02:30:00Z"},{"value_inc_vat":5,"valid_from":"2024-01-01T03:00:00Z"},{"value_inc_vat":5,"valid_from":"2024-01-01T03:30:00Z"},{"value_inc_vat":5,"valid_from":"2024-01-01T04:00:00Z"},{"value_inc_vat":5,"valid_from":"2024-01-01T04:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T05:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T05:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T06:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T06:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T07:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T07:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T08:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T08:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T09:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T09:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T10:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T10:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T11:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T11:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T12:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T12:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T13:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T13:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T14:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T14:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T15:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T15:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T16:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T16:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T17:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T17:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T18:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T18:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T19:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T19:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T20:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T20:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T21:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T21:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T22:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T22:30:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T23:00:00Z"},{"value_inc_vat":30,"valid_from":"2024-01-01T23:30:00Z"}]}` 200
              }
            '';
          };
        };
      };

    temporal =
      { pkgs, ... }:
      {
        networking.firewall.allowedTCPPorts = [ 7233 ];

        environment.systemPackages = [
          pkgs.grpc-health-probe
          pkgs.temporal-cli
        ];

        services.temporal = {
          enable = true;
          settings = {
            log = {
              stdout = true;
              level = "info";
            };
            global.membership = {
              maxJoinDuration = "30s";
              broadcastAddress = "0.0.0.0";
            };
            services = {
              frontend.rpc = {
                grpcPort = 7233;
                membershipPort = 6933;
                bindOnIP = "0.0.0.0";
                httpPort = 7243;
              };
              matching.rpc = {
                grpcPort = 7235;
                membershipPort = 6935;
                bindOnLocalHost = true;
              };
              history.rpc = {
                grpcPort = 7234;
                membershipPort = 6934;
                bindOnLocalHost = true;
              };
              worker.rpc = {
                grpcPort = 7239;
                membershipPort = 6939;
                bindOnLocalHost = true;
              };
            };
            persistence = {
              defaultStore = "sqlite-default";
              visibilityStore = "sqlite-visibility";
              numHistoryShards = 1;
              datastores = {
                sqlite-default.sql = {
                  pluginName = "sqlite";
                  databaseName = "default";
                  connectAddr = "localhost";
                  connectProtocol = "tcp";
                  connectAttributes = {
                    mode = "memory";
                    cache = "private";
                  };
                  maxConns = 1;
                  maxIdleConns = 1;
                  maxConnLifetime = "1h";
                };
                sqlite-visibility.sql = {
                  pluginName = "sqlite";
                  databaseName = "default";
                  connectAddr = "localhost";
                  connectProtocol = "tcp";
                  connectAttributes = {
                    mode = "memory";
                    cache = "private";
                  };
                  maxConns = 1;
                  maxIdleConns = 1;
                  maxConnLifetime = "1h";
                };
              };
            };
            clusterMetadata = {
              enableGlobalNamespace = false;
              failoverVersionIncrement = 10;
              masterClusterName = "active";
              currentClusterName = "active";
              clusterInformation.active = {
                enabled = true;
                initialFailoverVersion = 1;
                rpcName = "frontend";
                rpcAddress = "temporal:7233";
                httpAddress = "temporal:7243";
              };
            };
            dcRedirectionPolicy.policy = "noop";
          };
        };

        virtualisation.cores = 2;
      };

    worker =
      { nodes, pkgs, ... }:
      {
        # Redirect api.octopus.energy to the Caddy mock on the `api` node.
        networking.extraHosts = "${nodes.api.config.networking.primaryIPAddress} api.octopus.energy";

        # Trust the test CA so reqwest (rustls-native-roots) accepts Caddy's cert.
        security.pki.certificates = [ (builtins.readFile "${testCerts}/ca.crt") ];

        systemd.services.gridshift-worker = {
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];
          serviceConfig = {
            ExecStart = "${gridshift}/bin/worker";
            Environment = [
              "TEMPORAL_ADDRESS=temporal:7233"
              "TEMPORAL_NAMESPACE=gridshift"
              # Exercise the SDK's TEMPORAL_API_KEY path. This test Temporal runs without an
              # authorizer, so the Bearer header is accepted and ignored. TEMPORAL_TLS=false
              # is required: the SDK auto-enables TLS when an api key is set, but the server
              # here is plaintext.
              "TEMPORAL_API_KEY=test-api-key-token"
              "TEMPORAL_TLS=false"
              "GRIDSHIFT_PROVIDER=octopus"
              "OCTOPUS_API_KEY=test-api-key"
            ];
            Restart = "on-failure";
            RestartSec = "2s";
          };
        };
      };

  };

  testScript = ''
    import json

    start_all()

    # Wait for each service to be up before proceeding.
    api.wait_for_unit("caddy.service")
    api.wait_for_open_port(443)

    temporal.wait_for_unit("temporal.service")
    temporal.wait_for_open_port(7233)
    temporal.wait_until_succeeds(
      "grpc-health-probe -addr=localhost:7233 -service=temporal.api.workflowservice.v1.WorkflowService"
    )

    # Namespace and search attribute setup, run from the temporal node.
    # Use wait_until_succeeds: the health probe passes before all internal
    # services are ready to accept RPC calls.
    temporal.wait_until_succeeds(
      "temporal operator namespace create --namespace default --address 127.0.0.1:7233",
      timeout=60
    )
    temporal.succeed(
      "temporal operator namespace create --namespace gridshift --address 127.0.0.1:7233"
    )
    temporal.succeed(
      "temporal operator search-attribute create --namespace default --address 127.0.0.1:7233 --name EnergyIntensive --type Bool"
    )

    # Create a managed schedule tagged with EnergyIntensive=true.
    # wait_until_succeeds: the search-attribute namespace mapping may not
    # propagate immediately after creation.
    temporal.wait_until_succeeds(
      "temporal schedule create --namespace default --address 127.0.0.1:7233 "
      "--schedule-id managed-test "
      "--workflow-id managed-test-wf "
      "--type FakeWorkflow "
      "--task-queue fake "
      "--interval 24h "
      "--schedule-search-attribute 'EnergyIntensive=true'",
      timeout=30
    )

    # Set up the restricted namespace. EnergyIntensive is intentionally NOT
    # registered here. When gridshift calls discover_schedules for this
    # namespace with query "EnergyIntensive = true", Temporal returns an error
    # (unknown search attribute); the activity catches it, logs a warning, and
    # returns empty. The schedule below is therefore never discovered or updated.
    temporal.wait_until_succeeds(
      "temporal operator namespace create --namespace restricted --address 127.0.0.1:7233",
      timeout=30
    )
    temporal.wait_until_succeeds(
      "temporal schedule create --namespace restricted --address 127.0.0.1:7233 "
      "--schedule-id restricted-test "
      "--workflow-id restricted-test-wf "
      "--type FakeWorkflow "
      "--task-queue fake "
      "--interval 24h",
      timeout=30
    )

    worker.systemctl("start network-online.target")
    worker.wait_for_unit("network-online.target")
    worker.systemctl("start gridshift-worker.service")
    worker.wait_for_unit("gridshift-worker.service")

    # Create the gridshift Temporal schedule in the gridshift namespace.
    worker.wait_until_succeeds(
      "TEMPORAL_ADDRESS=temporal:7233 TEMPORAL_NAMESPACE=gridshift "
      "TEMPORAL_API_KEY=test-api-key-token TEMPORAL_TLS=false "
      "GRIDSHIFT_QUERY='EnergyIntensive = true' "
      "GRIDSHIFT_TIMEZONE=UTC "
      "${gridshift}/bin/starter",
      timeout=30
    )

    # Wait for the managed schedule to be indexed in visibility so
    # discover_schedules finds it when the gridshift workflow runs.
    temporal.wait_until_succeeds(
      "temporal schedule list --namespace default --address 127.0.0.1:7233 --query 'EnergyIntensive = true' | grep -q managed-test",
      timeout=600
    )

    # Trigger the gridshift schedule immediately rather than waiting until 16:00.
    temporal.succeed(
      "temporal schedule trigger --namespace gridshift --address 127.0.0.1:7233 --schedule-id gridshift"
    )

    # Wait for the SchedulerWorkflow to complete.
    def wf_completed(_):
      out = temporal.succeed(
        "temporal workflow list --namespace gridshift --address 127.0.0.1:7233 --output json"
      )
      wfs = json.loads(out)
      return any(
        w.get("type", {}).get("name") == "SchedulerWorkflow"
        and w.get("status") == "WORKFLOW_EXECUTION_STATUS_COMPLETED"
        for w in wfs
      )

    retry(wf_completed, timeout=120)

    # Verify the managed schedule's interval phase has been shifted into the cheap
    # window (02:00-04:30 UTC = 7200-16200 seconds).
    def spec_updated(_):
      desc = json.loads(temporal.succeed(
        "temporal schedule describe --namespace default --address 127.0.0.1:7233 --schedule-id managed-test --output json"
      ))
      spec = desc.get("schedule", {}).get("spec", {})
      intervals = spec.get("interval", [])
      if not intervals:
        return False
      phase_str = intervals[0].get("phase", "")
      if not phase_str:
        return False
      phase_secs = float(phase_str.rstrip("s"))
      return 7200 <= phase_secs <= 16200

    retry(spec_updated, timeout=30)

    # Verify restricted-test was NOT modified. Because EnergyIntensive is not
    # registered in the restricted namespace, discover_schedules returns empty
    # for that namespace and no update is attempted. The phase stays at 0.
    desc_restricted = json.loads(temporal.succeed(
      "temporal schedule describe --namespace restricted --address 127.0.0.1:7233 --schedule-id restricted-test --output json"
    ))
    spec_restricted = desc_restricted.get("schedule", {}).get("spec", {})
    intervals_restricted = spec_restricted.get("interval", [])
    assert intervals_restricted, "restricted-test schedule has no interval spec"
    phase_str_r = intervals_restricted[0].get("phase", "0s")
    phase_secs_r = float(phase_str_r.rstrip("s")) if phase_str_r else 0.0
    assert not (7200 <= phase_secs_r <= 16200), (
      f"restricted-test schedule was unexpectedly updated to phase {phase_secs_r}s "
      "(discover_schedules should have returned empty for the restricted namespace)"
    )
  '';
}
