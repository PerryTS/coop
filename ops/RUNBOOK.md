# Perch alert runbook

This runbook maps one-to-one to the alerts in
[`prometheus/perch-rules.yml`](prometheus/perch-rules.yml). Alertmanager routing
belongs to the environment: route `severity="critical"` to the primary on-call
and `severity="warning"` to the service owner, with deployment, shard slot,
queue, and operation labels preserved. Never attach request bodies, queue
payloads, credentials, or database URLs to alerts.

Start every investigation by recording the Perch/Perry build identity, host,
deployment package identity, alert start time, recent deployment operation,
and relevant Grafana panel. Use the authenticated deployment health, memory,
and artifact endpoints when the alert carries a deployment label. Prefer a
verified rollback or removal of new traffic over editing immutable packages or
queue rows by hand.

## PerchMetricSurfaceMissing

- Confirm the scrape target and `/metrics` endpoint independently of the
  application listener, then distinguish daemon absence from scrape failure.
- Check daemon/service-manager exit state, provider integrity errors, listener
  bind failures, and host resource exhaustion.
- Restore the last verified daemon/configuration. Escalate immediately if the
  process will not remain ready or provider verification fails.

## PerchHttpErrorRateHigh

- Break down status, latency, admission, deadline, and worker-restart panels for
  the labeled deployment; compare the alert start with its active package.
- If errors began with a replacement, run the authenticated verified rollback
  and confirm activation health before restoring traffic.
- If every deployment is affected, investigate provider, daemon, database, and
  host capacity before changing application code.

## PerchExecutorQueueSaturated

- Compare queue depth/capacity with admitted concurrency, latency, timeouts,
  CPU, and application arena growth.
- Shed or rate-limit traffic before increasing bounds. Raising queue capacity
  delays rejection and consumes memory; it does not create execution capacity.
- Move untrusted or blocking work to dedicated isolation and profile the slow
  entry point before changing the production limit.

## PerchWorkerTransportBacklogged

- Check the deployment's worker PID/generation, transport poison count,
  timeouts, protocol throughput, and executor queue.
- A continuously occupied ordered connection usually means a stuck invocation
  or an unhealthy generation. Confirm replacement completes and the backlog
  returns to zero.
- If replacement repeats, drain traffic and inspect worker stderr/cgroup events
  before retrying another generation.

## PerchWorkerTransportCancelledActive

- Treat this as an uncertain protocol exchange. Confirm the exact worker or
  shard generation was poisoned and replaced; do not reuse its connection.
- Correlate the entry point with client cancellation, daemon task failure,
  shutdown, and deadline logs.
- For a shard, verify every resident deployment recovered because the complete
  failure domain is retired after uncertain framing.

## PerchAdmissionRejections

- Split by entry point and reason, then compare admitted concurrency with
  executor and worker-transport occupancy.
- Verify callers receive the documented stable overload/size response and that
  rejected work was never dispatched.
- Apply upstream backpressure or reduce per-request work before increasing a
  bound; retain memory and tail-latency measurements for any limit change.

## PerchInvocationTimeouts

- Identify trusted, dedicated, or sharded isolation before acting. Trusted
  native work cannot be safely hard-preempted and retains its admission permit.
- Dedicated timeout recovery must replace one worker; sharded timeout recovery
  must retire and restore the whole shard generation.
- Drain traffic if recovery does not converge, then inspect the timed-out entry
  point, cgroup events, poison reason, and immediate successor generation.

## PerchActivationUnhealthy

- Read the deployment health endpoint for probe path, expected/actual status,
  request count, duration, and completion time.
- Confirm the previous generation is still serving. Fix or discard the
  candidate; never force-publish a generation that failed its packaged probe.
- Roll back only to a retained package that passes integrity verification and
  the same eager activation contract.

## PerchWorkerCrashLoop

- Inspect restart reasons, worker stderr, exit status, cgroup OOM/PID events,
  package identity, and the first failed generation.
- Stop automated traffic/reload pressure if backoff is not containing impact.
- Roll back a new package when failures align with activation; otherwise
  isolate the app and preserve the failing artifact and logs for diagnosis.

## PerchShardFailure

- List every deployment and runtime ID resident in the labeled slot/generation;
  verify all are moved together to one healthy successor generation.
- Check the initiating poison/timeout/OOM reason and sibling route availability.
- Repeated failures require draining that shard and moving suspect applications
  to dedicated isolation before resuming normal placement.

## PerchWorkerOomKilled

- Read `memory.current`, `memory.events`, peak memory, PID count, and the
  deployment/shard arena and response-size panels for the failed generation.
- Do not blindly raise the cgroup limit. First verify response/payload bounds,
  concurrency, application retention, and whether the limit covers a shard.
- Roll back or isolate the workload; change a limit only with retained PSS,
  cgroup peak, latency, and error evidence.

## PerchQueueBacklogOld

- Check visible, leased, retry-scheduled, and dead-letter counts plus oldest
  age, active consumer generation, admission deferrals, and delivery outcomes.
- Verify the deployment and PostgreSQL pool are healthy before increasing
  concurrency. Expired leases should be recovered by the current generation.
- Preserve at-least-once semantics: do not manually delete or acknowledge rows
  to clear the alert.

## PerchQueuePoolSaturated

- Compare maximum, open, available, checked-out utilization, and waiter count;
  correlate with database latency/errors and deployment queue concurrency.
- Find leaked/slow borrowers or a degraded PostgreSQL service before raising
  pool size. Increasing it can transfer overload to the database.
- Verify pool reconnect and waiter recovery after remediation.

## PerchQueueDeadLetters

- Use the authenticated metadata-only DLQ listing to identify deployment,
  queue, attempts, and failure timing without exposing payloads.
- Fix the consumer or dependency first. Replay only the selected bounded set
  through the audited control and verify acknowledgements/retry counts.
- Purge only under the application's data-retention procedure with an operator
  record; DLQ presence is not itself permission to discard work.

## PerchQueueStoreErrors

- Break down by operation and correlate with pool state, PostgreSQL health,
  connection replacement, schema/version, and network events.
- The service must fail closed for queue-backed deployments when durable state
  is unavailable. Confirm HTTP-only deployments remain isolated from the fault.
- Restore database service/connectivity, then verify reconnect, lease recovery,
  and delivery before clearing the incident.

## PerchRollbackFailed

- Inspect the requested package, integrity/config/static verification result,
  activation probe, and current active/previous state.
- Keep the current healthy generation live. Never edit the state file or
  immutable package to bypass verification.
- Select another verified retained generation or deploy a fixed candidate; if
  state integrity is uncertain, stop mutation and preserve the artifact tree.

## PerchArtifactCollectionFailed

- Inspect the deployment namespace, active/previous/live pins, retention
  policy, filesystem capacity/permissions, and rejected symlink or malformed
  tree warning.
- Collection failure is not permission for recursive manual deletion. Restore
  capacity or permissions and rerun authenticated collection.
- If the tree is malformed, quarantine the host from deployment mutations and
  preserve it for integrity investigation before any removal.
