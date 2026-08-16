# Perry application-library server plan

This is the canonical, living plan for the Perry/Coop multi-application
server. It includes the target architecture, what is implemented, every known
remaining workstream, the benchmark contract, production gates, and the work
that can proceed independently of Next.js compatibility.
[SHARED_RUNTIME.md](SHARED_RUNTIME.md) is the lower-level library contract and
[BENCHMARKS.md](BENCHMARKS.md) is the measurement record; when those documents
disagree with this one, the discrepancy must be resolved rather than silently
choosing the more favorable claim.

**Last reviewed:** 2026-08-14 against Coop's current working tree, pinned
Perry `0.5.1503` (`564c563…`), and the clean Perry `main` candidate
`3381e1b…`.

Status language is deliberately strict:

- **Done** means the implementation and its stated local correctness tests
  exist. It does not imply production evidence.
- **Implemented; evidence pending** means the mechanism exists, but the
  controlled Linux, failure-injection, or soak gate has not yet been retained.
- **Blocked** means a measured defect prevents the next claim; it is not a
  synonym for merely unfinished work.
- **Production-ready** is reserved for a workstream whose correctness,
  efficiency, reliability, and reproducibility gates all pass under the chosen
  isolation topology.

Every completed item must leave reproducible evidence tied to exact Coop,
Perry, provider, application, configuration, toolchain, target, and benchmark
identities. A benchmark improvement may not weaken response validation,
readiness semantics, artifact integrity, or isolation without being reported
as a different server shape.

## Executive position

The intended end state is one Coop service hosting many separately deployed
Perry application libraries while sharing one Perry runtime and one stdlib
image. Applications are compiled and validated before activation, kept warm,
and invoked directly through a strict binary ABI. This can remove process-per-
application memory duplication and eliminate compilation, process creation,
dynamic loading, binding, and module initialization from the request path.

The current evidence supports the architecture, but not yet the unconditional
claim that Perry beats Node.js and celld everywhere:

- Perry already has a strong density advantage over one Node process per tiny
  application and has low first-request overhead after eager activation.
- A consolidated Node process remains the harder memory comparison. Native
  Linux Perry wins warm PSS at one and ten tiny apps, but loses at one hundred
  because its per-app executor/thread-local/arena state grows while the
  consolidated Node control stays nearly flat.
- Perry cannot be called a faster Next.js host until the real production Next
  pipeline passes without fixture-only shortcuts.
- The equivalent Worker-shaped five-trial matrix is now complete for Perry,
  plain Node, and celld. With a root-owned immutable provider package Perry
  beats Node usable startup (55.6 versus 147.8 ms) and both competitors' warm
  PSS (28.24 versus 30.14/31.45 MiB). celld starts fastest (23.0 ms usable),
  and Node still wins CPU/request (87.5 versus Perry's 118.5 us). celld is not
  a Next.js host, and its lazy activation remains visible as a separate first-
  request measurement.
- The five-trial Worker mechanism run and two-trial 1/10/100 density run retain
  PSS, private dirty, cgroup memory, CPU, exact providers, and exact server
  binaries. A repeat on the selected production Linux/Node version and a green
  reproducible CI run are still required before making production claims.

The target verdict is therefore not a slogan. “Perry wins” is earned only when
the same validated workload, isolation topology, readiness definition, and
measurement protocol show lower usable startup, warm memory, and server CPU
without worse errors or tail latency.

### Plan at a glance

| Lane | Present state | Remaining exit condition |
|---|---|---|
| Shared-library mechanism | Runtime and stdlib are separate, apps are app-only, ABI v2 is strict, activation is eager, and the provider/app boundary passes natively on Linux | Preserve the contract on the promoted Perry revision and retain a green isolated CI run |
| Deployment pipeline | Exact local/dependency/config/static snapshots, deterministic bounded compilation, immutable packages, rollback, restart restoration, retention, GC, byte-identical live-runtime reuse, and real OS-SIGKILL recovery from compiler launch through validated staging, warm activation, package/state publication, and trash collection are implemented | Repeat the boundary matrix on native Linux, run the long lifecycle soak, and close the Perry Buffer-retention failure it exposed |
| Runtime density | One process can host 100 warm application libraries with explicit executor/arena controls and per-deployment arena telemetry. Native Linux Perry beats consolidated Node warm PSS at 1/10 tiny apps and loses at 100; it wins CPU/request in all three diagnostic rows | Stop old-generation host-ABI Buffer growth, reduce per-app private state enough to beat consolidated Node at 100 apps, and repeat with five trials |
| Background work | Binary cron/queue ABI, explicit UTC overlap/lateness/no-catch-up cron policy, cron generation lifecycle, and the host-owned Postgres queue service pass real-daemon delivery, retry/DLQ, killed-delivery recovery, restart, replacement, rollback, backend termination, full database stop/start, and operator-control proof. Provider enqueue plus raw-byte delivery are proven in trusted, dedicated, and sharded modes; scheduled cron also passes through a real shard | Complete the long queue concurrency/pool/PSS soak and repeat the stop/start gate on native Linux |
| Reliability and isolation | Admission, byte limits, deadlines, worker poisoning/replacement, rollback, explicit per-deployment `trusted`/`sharded`/`dedicated`/`inherit` policy, bounded deterministic process shards, cgroup-v2 worker/shard enforcement, exact-parent worker binding, idle-exit detection, generation-safe repeated restart, and bounded restart backoff are implemented. A real local daemon test proves hard-timeout recovery followed immediately by another process failure in both dedicated and two-app sharded modes | Repeat that matrix with delegated Linux cgroups, retain the broader failure/saturation soak, and choose the production isolation default |
| Operations and evidence | Core metrics, metric-checked Prometheus recording/alert rules, synthetic rule tests, one-to-one alert runbooks, enforced bounded-label policy, a live-proven Grafana dashboard/datasource, and reproducible harnesses exist. Native Perry/Node tiny-app evidence and a five-trial identical Perry/Node/celld Worker matrix are retained with exact environment identities | Configure fleet receivers, retain a green CI run, repeat the cost/provisioning gates and both matrices on the selected production Linux/Node version |
| Next.js compatibility | Independently owned in Perry issues #8034–#8040 | Pass the unmodified production App Route oracle before making a full-Next verdict |

The independent server lane does not wait for Next.js compatibility. It should
finish deployment safety, durable queues, isolation, Linux proof, and the
mechanism benchmark while the Perry compatibility workers close the framework
gates. The final full-Next comparison joins the two lanes only after both are
green.

The highest-priority measured blocker in that independent lane is now Perry GC
behavior at the host ABI. A 50,000-request churn probe retains exactly 152
old-generation Buffer bytes per invocation and does not trigger an automatic
full collection. A host-forced full collection is not a safe workaround: in
the pinned runtime it can corrupt the next ABI response. The fix belongs in
Perry's Buffer rooting/collection or GC pacing contract and has an ignored,
currently failing Coop promotion test. This is independent of Next.js
compatibility and must be green before the density verdict can be production-
worthy.

## Objective

Build a server in which one Coop executable hosts many independently compiled
Perry applications with lower startup time, memory consumption, and compute
cost than the honest Node.js and celld alternatives.

The intended production shape is not “spawn Perry on demand.” Applications are
compiled ahead of request handling, deployed as application-only shared
libraries, eagerly loaded and initialized, and then invoked through a small
versioned binary ABI. A routed request must never compile, spawn a process,
`dlopen` an image, bind symbols, or initialize a module.

“Zero latency” in this plan means zero cold-start work on the request path. It
does not mean zero execution time: routing, a channel handoff, ABI encoding,
application work, and response decoding still consume CPU.

## Scope and non-goals

This plan covers provider packaging, app-only compilation, immutable
deployment artifacts, eager activation, HTTP/cron/queue invocation, hot
replacement, observability, resource policy, isolation choices, Linux
packaging, and fair Perry/Node/celld measurements. It includes the Next.js
compatibility track because that track is necessary for the final product
claim, even though separate workers currently own its compiler/runtime fixes.

The plan does not promise:

- literally zero request execution time;
- compilation or lazy application activation on the first request;
- Node API compatibility beyond the explicitly supported application surface;
- process-grade security or crash containment from trusted in-process mode;
- exactly-once queue execution in the presence of crashes;
- that a framework-shaped fixture is equivalent to the real Next.js runtime;
- that a comparison against 100 isolated Node processes proves a win against
  one defensible consolidated Node service.

## Terms and measurement boundaries

- **Provider pair:** the exact `libperry_runtime` and `libperry_stdlib` files,
  manifest, ABI identity, toolchain identity, and allocator/arena profile.
- **Application package:** one immutable content-addressed directory containing
  an app-only shared library and its adjacent ABI/integrity manifest.
- **Activation:** integrity and boundary validation, `dlopen`, symbol binding,
  executor creation, module initialization, warmup/health validation, and
  publication to the live router.
- **Fresh deployment activation:** activation of newly produced file identities.
  This includes platform loader work and is not the same as restart.
- **Restart:** activation over the same retained immutable provider and app
  files after starting a new Coop process.
- **Listener-ready:** every advertised deployment is already invokable; no
  application activation is deferred to the first request.
- **Usable cold start:** process start through the first validated response.
- **Ready memory:** memory after eager activation and before measured traffic.
- **Warm memory:** memory after every advertised application has completed the
  defined warmup/first-request workload.

## Non-negotiable properties

1. `perry-runtime` and Perry stdlib are separate provider files.
2. Application libraries contain application code only. They may import from
   the providers but may not embed or redefine provider-owned code.
3. Runtime, stdlib, compiler, and applications have an exact Perry revision,
   toolchain, target, and ABI identity.
4. Runtime and stdlib load once per host process; every application reuses
   those process-wide images.
5. Every application is loaded, bound eagerly, initialized, and warmed before
   it is published to the router.
6. HTTP uses the strict binary ABI v2. There is no manifestless, ABI-v1,
   JSON/Base64, or other legacy application fallback.
7. Deployments remain separate artifacts with independent routing, lifecycle,
   state, and replacement.
8. Benchmarks compare equivalent work and report first deployment activation,
   process restart, and first request separately.

## Target architecture

```text
Coop server process
├── libperry_runtime.{dylib,so}   loaded once, RTLD_NOW | RTLD_GLOBAL
├── libperry_stdlib.{dylib,so}    loaded once, bound to that runtime
├── router and lifecycle supervisor
└── application executors
    ├── app-001.{dylib,so}          application code + exact configured ABI exports
    ├── app-002.{dylib,so}
    └── ...

Request
  → router lookup
  → executor handoff
  → COOP Buffer call
  → native application execution
  → COOP Buffer response
```

Perry runtime state is currently thread-local, so each loaded application is
pinned to one executor thread. That is a correctness constraint today, not a
permanent architectural preference. The memory and scheduling cost of this
model is one of the principal optimization workstreams.

### Request path

```text
accepted connection
  → immutable router snapshot lookup
  → per-deployment admission and deadline
  → encode strict COOP request
  → bounded executor handoff
  → already initialized application call
  → decode and validate COOP response
  → record bounded-cardinality metrics
  → send response
```

The request path may not compile, inspect source trees, validate package hashes,
load providers, `dlopen` an app, resolve exports, initialize a module, start a
worker, or rebuild routing state. Admission rejection must be immediate when a
deployment has exhausted its configured concurrency or queue budget.

### Deployment and replacement state machine

```text
source/config change
  → take deterministic source/config snapshot
  → compute source + compiler + provider + ABI identity
  → reuse exact immutable package OR compile in same-filesystem staging
  → audit dependencies/exports and verify exact bytes
  → fsync package contents and atomically rename into content namespace
  → preload app, bind exports, create executor, initialize, warm, health-check
  → validate cron/queue configuration while still inactive
  → publish router/runtime generation atomically
  → activate new background work and stop old generation from claiming work
  → drain old in-flight calls with bounded grace
  → retain rollback generations, then garbage-collect unreferenced packages
```

Every failure before publication leaves the old generation live and removes
incomplete staging data. Every failure after publication must either roll back
to a known-good retained generation or mark the deployment unhealthy without
corrupting unrelated deployments. The implemented path already stages,
validates, atomically publishes, eagerly initializes, swaps routing, gates cron
activation, drains the old executor, persists active/previous package identity,
and provides verified rollback plus retention-aware collection. Bounded
activation warmup/health policy, full-daemon retry/DLQ behavior, rollback
leadership, killed-delivery recovery, and durable crash-image recovery are also
implemented. A real debug-daemon matrix now pauses at compiler launch,
fully-validated staging, and post-probe/pre-state activation; sends SIGKILL;
proves the old generation is retained; verifies the parent-bound compiler is
gone; reconciles dead-owner staging immediately; restarts from the exact old
package; and then promotes the candidate. Together with the package/state/trash
matrix, every durable deployment boundary has local OS-process proof. A
disposable pinned-Postgres gate also stops the complete database container,
proves no enqueue is falsely acknowledged during the outage, restarts it, and
proves producer and consumer recovery without a daemon restart. The remaining
gaps are native-Linux repetition and the long-running daemon, queue, pool, and
collection soaks.

### Artifact layout

```text
providers/
  <provider-identity>/
    libperry_runtime.{dylib,so}
    libperry_stdlib.{dylib,so}
    provider-manifest.json

compiled/
  <deployment>/
    .coop-deployment-state.json
    <package-sha256>/
      app.{dylib,so}
      app.coop-lib.json
      deployment.coop.json
      static.coop-manifest.json
      .coop-static/
        <configured-root-index>/...
```

The provider location above is the production package target; development
currently uses the configured `var/coop/lib` paths. Automatic application
compilation already publishes into the content-addressed application layout.
Source-less deployments may restore only a previously published, verified,
content-addressed package. The mutable `compiled/<name>.<ext>` legacy fallback
is removed.

## Current status as of 2026-08-14

| Area | Status | Evidence or remaining work |
|---|---|---|
| Perry revision | Reproducible baseline pinned; latest-main promotion pending | `perry-main.lock` and `.perry-main` are clean at `0.5.1503` / `564c563…`. The clean candidate worktree matches current `main` at `3381e1b…`, 43 commits later, and contains the #8035, #8039, and #8041 GC changes. Its Buffer allocation path is unchanged, so promotion still requires the host-Buffer churn and forced-GC/root-safety gates rather than assuming `main` fixes it. |
| Perry Linux `Buffer.from(array)` compatibility | Defect isolated; server fixtures use portable construction | On the pinned Linux compiler, `Buffer.from([numeric literals])` returns an empty Buffer while `Buffer.alloc` plus indexed byte writes works. This initially looked like cron/queue dispatch corruption; retained wrapper disassembly and direct plugin calls proved the server routes and stable aliases are correct. Perry needs a focused source regression and fix before idiomatic binary applications can rely on this constructor. |
| Separate runtime and stdlib | Done on macOS and Linux | Both providers are independently packaged and loaded in order. Native Linux artifacts are 58,524,728-byte runtime and 36,188,992-byte stdlib files with retained SHA-256 identities. |
| App-only library boundary | Done for current fixtures on macOS and Linux | Apps import both providers, expose only module initialization plus stable Coop-owned HTTP/cron/queue aliases, and are boundary-audited. Linux ELF `NEEDED` and exact-export audits pass. Generated Perry path-derived wrapper names are not public ABI. |
| Strict HTTP ABI v2 | Done | Raw request/response bytes use `COOP`; legacy fallback is removed. |
| Eager activation | Done | Listener/router publication follows provider load, app load, module initialization, and the packaged bounded HTTP warmup/health contract. Failed probes drain the unpublished generation and preserve the current one; exact healthy reloads are no-ops; live outcome/status/duration is queryable and metricized. |
| Artifact integrity | Done for apps, providers, configuration, and static assets | App, runtime, stdlib, exact deployment configuration, and immutable static snapshots are tied to size/SHA-256 manifests and fail closed before loading. Static symlinks, special files, file-count overflow, and byte-limit overflow are rejected. |
| Hot replacement | Atomic routing and live-runtime reuse proven; memory gate failing | Byte-identical application images reuse the healthy initialized runtime after a fresh activation probe, while configuration, immutable package identity, routing, admission, and background generation still change atomically. A 250-cycle full-daemon soak achieved 250/250 runtime reuses and 21,740 concurrent validated requests with zero errors, no compiler, no second native app load, three retained packages, threads 13→13, and FDs 13→14. RSS grew 8,464 KiB with a 34.2-KiB/cycle latter-half slope; arena telemetry attributes the retained trend to Perry host-ABI Buffers, so the soak is correctness-green but memory-red. |
| Symbol/debug stripping | Done on macOS and Linux | macOS providers total 97.8 MB instead of 111.1 MB; the tiny app is 51,720 bytes. Native Linux providers total 94,713,720 bytes. |
| Many-app measurement | Native Linux diagnostic evidence retained | Fresh-image activation and ordinary restart are reported separately. A 100-app native run reached 57.4 MiB ready PSS and 99.3 MiB warm PSS; a separate 250-cycle native executor lifecycle gate kept threads 3→3 and FDs 10→10. |
| Linux code paths | Native bring-up green; CI evidence pending | Pinned providers build, ELF boundaries pass, exact immutable packages restore without the removed legacy fallback, direct loader/ABI/guard/crash tests pass, auto-compile HTTP/cron/queue passes, the 250-cycle lifecycle and 100-app capacity gates pass, and release metric cost is 149 ns/request. `linux-shared-runtime.yml` reproduces these paths; an external green workflow artifact remains required. |
| Linux PSS/cgroup benchmark accounting | Implemented; density and identical-Worker native evidence retained | The two-trial Perry 1/10/100 and consolidated/isolated Node density matrices plus the five-trial identical Perry/Node/celld Worker matrix report PSS, private dirty, cgroup current/peak, and `/proc`-based CPU. The launcher fails closed on incorrect cgroup membership. Raw results and exact machine/toolchain/workspace/provider/server identities are in `benchmarks/results/2026-08-14-linux-*.txt`; production-host repetition remains. |
| Full Next.js execution | In progress | Focused #8035 and #8039 fixes have landed upstream; #8034's full production provider-hosted oracle is still red, and #8036–#8038 plus tracker #8040 remain open. Compatibility workers own these gates. |
| Executor density controls | Implemented | Stack size and bounded command-queue capacity are explicit configuration; overload rejects rather than growing memory without bound. |
| Perry memory attribution | Implemented; defect isolated | Per-application live/reserved arena statistics are exposed through authenticated admin status and Prometheus gauges. A 100-cycle replacement soak grew live arena bytes from 2,640 to 2,242,048 and reserved bytes from 262,144 to 2,490,368; a separate 50,000-request gate proves linear 152-byte-per-request old-generation Buffer retention. |
| Cron and queue binary ABI | Functional lifecycle including shard queue is locally proven; soak pending | Strict `COOP` frames, stable exact exports, JSON and raw-byte host-owned enqueue, explicit UTC cron overlap/lateness/no-restart-catch-up policy, Postgres leasing/retry/DLQ, lifecycle consumers, metrics, and authenticated DLQ operations are implemented. Real scheduled cron crosses the shard runtime-ID protocol. A disposable-Postgres real-daemon run passes trusted and dedicated modes across daemon restart, hot replacement, rollback, a killed in-flight delivery, forced database-connection replacement, then proves provider-callback enqueue and exact raw-byte acknowledgement through a real shard runtime ID. |
| Per-deployment admission/deadlines | Core enforcement implemented | HTTP, cron, and queue share one non-waiting deployment semaphore capped by executor capacity. Request/header/response/queue/ABI byte limits fail before executor work where possible; HTTP maps overload, size, and deadline failures to stable 503/413/431/502/504 responses. Timed-out native work retains its permit. Worker transport is poisoned and its unique generation replaced after an uncertain timeout/framing failure. Dedicated workers have cgroup memory/CPU/PID enforcement; hard in-process preemption remains impossible. |
| HTTP hot path | Lock and allocation reductions implemented; controlled production-host A/B pending | Router reads use an atomic immutable snapshot, routing borrows request metadata, immutable route limits avoid a second live-state read, and admission/request metric handles are cached. A phase trace attributes roughly 1.4 us to COOP encode/decode and roughly 42 us to the Perry invocation in the tiny Worker shape. Shared-VM reruns were too load-sensitive to claim a CPU win, so the retained five-trial matrix remains the verdict baseline. |
| Artifact rollback and GC | Core implementation, daemon integration, durable crash-image proof, and full local deployment-boundary SIGKILL matrix complete | Packages include exact configuration and static bytes; activation state records active and previous identities atomically; authenticated rollback re-verifies and eagerly activates the selected package; restart restores that exact selection without recompiling mutable source; count/age retention, live pins, symlink-safe collection, trash staging, startup reconciliation, status, and metrics are implemented. Child-process tests cover compiler start, validated staging, successful activation probe before state publication, package publication, synced state temporary file, state rename, and trash rename. Real SIGKILL proves startup selects an old or new complete generation and reconciles only unreachable data. Native-Linux repetition and the long daemon collection/replacement soak remain. |
| Compilation controls | Deterministic bounded path and daemon-crash containment implemented; controlled evidence in progress | Compilation has independent bounded concurrency, queue-time metrics, wall/RSS budgets, bounded diagnostics, exact local and dereferenced dependency snapshots, mutation detection, explicit compiler argv/environment/provider identity, a stable Coop-owned Perry object cache, package/compiled-image reuse telemetry, phase/failure metrics, and `coop build`. Timeout/RSS enforcement kills the private process group, and a parent-watching guard also kills that group when daemon SIGKILL bypasses destructors. The real compiler path and an OS parent-kill regression pass. Controlled cold/no-op/incremental/static/activation/rollback trials are reproducible; Linux and long cache/lifecycle soak remain. |
| Durable queues and database schema | Trusted/dedicated/sharded functional gate complete; soak pending | Coop owns migrations, deployment identity, transactional leases, retry/backoff, DLQ, consumer generations, admission, policy, metrics, authenticated metadata-only list/replay/purge operations, and JSON/raw-byte producer APIs. The real Postgres store and daemon suites prove trusted/dedicated/sharded execution, restart, replacement, rollback leadership, retries, explicit and exhausted DLQ, byte preservation, killed-delivery lease recovery, and pool reconnect. Queue deployments fail closed without a store by default. |
| Production isolation policy | Trusted/sharded/dedicated policy implemented; Linux shard evidence pending | `[isolation].class` explicitly selects trusted in-process, deterministic bounded multi-app shards, or one supervised dedicated worker, while `inherit` resolves the box default. Shards have stable hash preference with deterministic capacity probing, maximum distinct-app capacity, idempotent generation-scoped load/unload, aggregate cgroup memory/CPU/PID limits, sibling-safe unload, group failure detection, and group restart. Uncertain load outcomes retire the complete failure domain instead of leaking unreachable runtimes. Every daemon-spawned worker is bound to the exact daemon parent. A real two-app daemon test proves shared residency, crash-wide replacement, response recovery, independent unload, and idle-shard death after daemon SIGKILL. Retained delegated-cgroup Linux timeout/saturation/soak evidence and the production default decision remain. |

Current tiny-app controls on the loaded development machine:

- one new app image activated in approximately 405 ms;
- restarts over that same eagerly bound image took 97–123 ms;
- ready RSS was about 16.5 MiB and warm RSS about 18.3 MiB;
- 100 newly created macOS images took 28.89 seconds to activate once;
- restarts over those same 100 images took 724 and 620 ms;
- the original 100-app daemon used about 59 MiB ready RSS and 131 MiB after
  all apps were invoked;
- two direct 100-app capacity probes with the dense provider profile used
  39.8–40.3 MiB ready RSS and 101.9–102.5 MiB after every app was invoked;
- those 100 apps reserved 12.5 MiB of Perry arenas ready and 25 MiB warm, with
  only 160 and 312 live arena bytes per tiny app respectively.

The 100-app restart result is operationally different from the 28.89-second
fresh activation. The latter is a macOS deployment/image-validation cost. Coop
still uses eager binding and does not transfer that work to the first request.

These macOS measurements are directional because the machine was under heavy
concurrent load.

### First native Linux engineering evidence

The retained 2026-08-14 Ubuntu 24.04 arm64 run used six logical CPUs, 12 GiB
RAM, no swap, cgroup v2, Perry `564c563…`, celld `3f22aed…`, and Node 18.19.1.
Each row below is the conservative upper median of two trials with 2,000
validated requests at concurrency 50. It proves native packaging and exposes
the next bottleneck, but the release gate still requires five isolated trials
and the selected production Node version.

| Tiny `ok` shape | Startup/restart | Warm PSS | Warm private dirty | CPU/request |
|---|---:|---:|---:|---:|
| Perry, 1 app | 702 ms | 23.6 MiB | 4.5 MiB | 90 us |
| Perry, 10 apps | 811 ms | 31.0 MiB | 11.9 MiB | 115 us |
| Perry, 100 apps | 1,442 ms | 99.3 MiB | 80.2 MiB | 95 us |
| Node, 1 process / 1 app | 548 ms | 46.7 MiB | 13.8 MiB | 300 us |
| Node, 1 process / 10 apps | 308 ms | 46.8 MiB | 13.9 MiB | 265 us |
| Node, 1 process / 100 apps | 442 ms | 47.7 MiB | 14.8 MiB | 235 us |
| Node, 10 processes / 10 apps | 1,129 ms | 172.5 MiB | 137.8 MiB | 655 us |
| Node, 100 processes / 100 apps | 8,061 ms | 1,407.5 MiB | 1,366.4 MiB | 1,800 us |

Perry therefore wins this tiny-handler CPU comparison, the consolidated Node
memory comparison at one and ten apps, and every process-isolated density row.
Consolidated Node wins 100-app warm memory by 51.6 MiB and starts faster at
1/10/100; Perry starts much faster than 100 independent Node processes. The
per-app executor, thread-local runtime state, and native arena pages are the
immediate density target.

The corrected five-trial equivalent-Worker matrix now runs that exact URL,
100-iteration checksum, and validated JSON response in all three hosts:

| Worker shape | Listener start | Usable cold | Warm PSS | Warm private dirty | CPU/request |
|---|---:|---:|---:|---:|---:|
| Perry, root-owned immutable providers | 47.3 ms | 55.6 ms | **28.24 MiB** | **6.50 MiB** | 118.5 us |
| Plain Node | 141.8 ms | 147.8 ms | 30.14 MiB | 13.79 MiB | **87.5 us** |
| celld | **20.6 ms** | **23.0 ms** | 31.45 MiB | 10.97 MiB | 117.0 us |

Perry wins warm memory and startup against Node; celld wins startup; Node wins
CPU/request. Perry and celld CPU are effectively tied in this shared-VM run.
celld still activates lazily, so its listener and first-response boundaries
remain separate. MinIO was external to the measured celld cgroup. The result
is mechanism evidence, not a Next.js verdict.

For small Perry cases, cgroup current is lower than PSS because Linux charges a
shared file-backed page to one cgroup rather than distributing it
proportionally; providers were already resident and charged elsewhere in the
VM. PSS and private dirty are the primary cross-shape comparison. Cgroup
current/peak remains an operational capacity signal when the complete host
lifecycle begins in a fresh parent cgroup.

The raw output and identities are retained in `benchmarks/results` as
`2026-08-14-linux-environment.txt`, `2026-08-14-linux-perry-resource.txt`,
`2026-08-14-linux-node-resource.txt`, and
`2026-08-14-linux-celld-resource.txt`, and the full comparable matrix plus
environment are `2026-08-14-linux-worker-mechanism-optimized.txt` and
`2026-08-14-linux-worker-mechanism-optimized-environment.txt`.

## Design decisions

### Fixed decisions

| Decision | Rationale |
|---|---|
| Runtime and stdlib are separate provider images | Makes shared ownership visible and prevents every app from embedding them. |
| Apps are strict app-only libraries | Keeps deployment artifacts small and makes the provider boundary auditable. |
| HTTP, cron, and queue use versioned binary frames | Avoids JSON/Base64 overhead in-process and makes malformed input fail closed. |
| There is no legacy ABI fallback | A fallback would weaken integrity, make behavior deployment-dependent, and preserve two hot paths. |
| Activation is eager and publication is last | No cold loader/compiler work can surprise the first request. |
| Packages are immutable and content-addressed | Failed builds cannot overwrite a good deployment and exact rollback becomes possible. |
| Perry TLS is thread-affine for now | This preserves runtime correctness until Perry exposes explicit independent contexts. |
| Compile and preload concurrency are separate | Compiler peak memory must not starve already serving deployments. |
| Linux PSS/cgroup data is the publication baseline | RSS alone double-counts shared mappings and macOS development results are not fleet evidence. |

### Execution and failure-domain choices

| Deployment class | Process topology | Hard-kill boundary | Failure scope | Resource enforcement | Intended use |
|---|---|---|---|---|---|
| `trusted` | Application executor inside the Coop daemon | None for synchronous native work | Entire daemon and every in-process app | Admission, byte limits, cooperative wall deadline, descriptive process/arena memory | Mutually trusted apps where minimum latency and maximum density matter most |
| `sharded` | A deterministic bounded group of apps in one supervised worker | Whole shard process | Every app assigned to that shard generation | Aggregate shard cgroup memory/CPU/PIDs plus per-app admission and byte limits | Default candidate for a dense fleet that still needs bounded crash domains |
| `dedicated` | One supervised worker per app | That app's worker process | One deployment | Per-worker cgroup memory/CPU/PIDs plus per-app admission and byte limits | Untrusted, crash-prone, or strict hard-deadline workloads |
| `inherit` | Resolves the box-wide `execution.mode` | Inherited | Inherited | Inherited | Fleet policy without duplicating configuration in every package |

The sharded implementation defaults to four lazily started hash slots, at most
25 distinct deployments per shard, and aggregate limits of 1 GiB RSS, 400% CPU,
and 256 PIDs. These are safe configuration defaults, not capacity conclusions.
Production values must come from Linux PSS/CPU/tail-latency and crash-recovery
evidence. Replacement generations of the same logical deployment may overlap
inside a shard so activation can finish before the old generation drains.

### Decisions that must be closed before production

1. **Isolation default.** Choose trusted in-process, a bounded set of process
   shards, or one worker per app for each trust class. Density and crash
   containment must be reported together.
2. **Hard deadlines.** Synchronous native code cannot be forcibly cancelled
   safely inside the daemon. Hard-kill guarantees therefore require a worker or
   shard boundary; in-process deadlines can reject/abandon responses but cannot
   honestly promise preemption.
3. **Durable queue production policy.** Host-owned deployment identity,
   authenticated metadata-only DLQ inspection/replay/purge, poison-message
   handling, retry policy, outage recovery, and JSON/raw-byte enqueue are
   implemented. Production must still set pool and concurrency capacity,
   retention/SLO defaults, operator authorization, alert thresholds, and the
   idempotency contract for externally visible side effects.
4. **Streaming ABI.** Keep HTTP v2 buffered unless realistic body sizes prove a
   need for v3 streaming. If needed, define backpressure, cancellation, and
   disconnect semantics as a new ABI rather than extending frames implicitly.
5. **Provider trust boundary.** `full_hash` is the portable default and hashes
   the two provider files in parallel. Linux production may select
   `root_owned_immutable` only for a package installed through the checked-in
   full-hash installer: the unprivileged process verifies the canonical package
   path, every ancestor, and all files are root-owned and not writable before
   loading. Writable development paths and a root-running daemon fail closed.
6. **Runtime contexts.** Determine whether Perry can replace thread-local
   singleton state with explicit per-app contexts. This decision controls the
   long-term executor/thread density ceiling.

## Workstream 1: exact provider and application boundary

### Completed

- Package runtime and stdlib as separate process-wide provider images.
- Build runtime with Perry's `stdlib` feature so fallback stdlib definitions do
  not bind ahead of the actual stdlib provider.
- Bind applications directly to the provider install names/SONAMEs.
- Export only `perry_module_init` and stable Coop-owned HTTP/cron/queue Buffer
  entry aliases from apps. Resolve Perry's path-derived wrapper names at link
  time so staging paths and compiler naming details are not application ABI.
- Reject apps that define provider-owned symbols or contain provider code.
- Strip local/debug symbols while preserving the required dynamic ABI.
- Pin compiler version, commit, compiler digest, Rust version, and target.
- Record and verify the exact runtime and stdlib size and SHA-256 before
  `dlopen`; reject a changed provider package.
- Hash the independent provider files concurrently in portable `full_hash`
  mode. Install a fully hashed Linux package into a content-addressed,
  root-owned namespace and permit the explicit `root_owned_immutable` fast
  mode only when an unprivileged process proves the complete canonical path is
  not writable. The default remains `full_hash`; dedicated and sharded workers
  inherit the same configured policy.
- Record the provider allocator and GC arena profile as provider identity.
- Repeated provider initialization in one process returns after canonical-path
  identity validation and does not rehash the already resident 98 MB pair.
- Add a native Ubuntu workflow that installs Perry's pinned LLVM 22 toolchain,
  packages the providers, checks ELF `SONAME`/`NEEDED` and exports, runs real
  application roundtrips, and uploads raw 1/10/100 RSS/PSS output.
- Execute that provider/app path natively: the pinned Linux provider pair,
  strict app dependencies/exports, loader roundtrip, auto-compile HTTP/cron/
  queue flow, lifecycle control, and 100-app capacity gate pass.

### Remaining

- Complete the promotion matrix against the already fetched clean Perry `main`
  candidate at `3381e1b…`: provider packaging, ABI/integrity, app roundtrip,
  auto-compile, lifecycle, tiny capacity, Buffer-reclamation, and #8034
  compatibility tests. Update `perry-main.lock` and the provider identity only
  after the complete candidate passes; never make a benchmark depend on the
  moving branch name.
- Obtain and retain a green run of the implemented ELF boundary/roundtrip CI
  workflow; the workflow definition alone is not production evidence.
- Fix Perry's Linux `Buffer.from([number, ...])` construction and add a focused
  compiler/runtime regression. Coop must not add a compatibility fallback;
  its current fixtures use `Buffer.alloc` and indexed writes only to keep the
  server gate independent of the compiler defect.
- Integrate the root-owned provider installer into production image/package
  construction and exercise replacement/rollback of the indivisible provider
  pair. A future signed package index may improve distribution, but may not
  weaken the runtime ownership/path checks.
- Establish a provider ABI report so accidental export changes fail CI.
- Decide how provider artifacts are published, cached, rolled back, and garbage
  collected as one indivisible pair.

### Acceptance

An application artifact must fail closed if its Perry identity, target, ABI,
handler exports, dependency boundary, or bytes do not match the deployed
provider package. Passing validation may not add per-request work.

## Workstream 2: full Next.js compatibility

The checked-in reproducer is `benchmarks/next-small`. Workers should extend or
minimize that fixture rather than inventing unrelated applications. Every fix
must include a small source-level reproduction and a regression test in Perry
or Coop, depending on which layer owns the failure.

The canonical Perry issue tracker is
[#8040](https://github.com/PerryTS/perry/issues/8040). Every ticket points to
the complete copy/paste application generator and 21-request verifier in
[#8034](https://github.com/PerryTS/perry/issues/8034), so a worker does not need
to design a Next application or its oracle.

| Issue | Current state | Owned acceptance boundary |
|---|---|---|
| [#8034](https://github.com/PerryTS/perry/issues/8034) | Open | Commit the pinned Next 16.3.0/React 19.2.4 production App Route fixture and app-only-dylib CI gate. |
| [#8036](https://github.com/PerryTS/perry/issues/8036) | Open | Preserve `NextRequest`, URL/query/header/body state across the production import/re-export boundary. |
| [#8039](https://github.com/PerryTS/perry/issues/8039) | Closed upstream via #8043; production gate still pending | The focused lazy path-module implementation and tests landed. #8043 explicitly did not pass the complete provider-hosted #8034 matrix, so the common production oracle must still validate it. |
| [#8037](https://github.com/PerryTS/perry/issues/8037) | Open | Preserve Next request/work async-local stores across concurrent continuations. |
| [#8038](https://github.com/PerryTS/perry/issues/8038) | Open | Return and drain the actual streamed `NextResponse`, including status, headers, and cookies. |
| [#8035](https://github.com/PerryTS/perry/issues/8035) | Closed upstream | Fix low-address macOS array-growth forwarding; the integration gate must still verify the selected Perry pin includes it. |

### Compatibility gates

1. Imported functions preserve request arguments across module boundaries.
2. `NextRequest.nextUrl.searchParams` behaves correctly.
3. Returned `Response` status, duplicate headers, body bytes, and streams cross
   compiled module boundaries.
4. Next's request and work `AsyncLocalStorage` stores execute correctly.
5. The production webpack lazy route-module boundary neither deadlocks nor
   loses values.
6. The selected Perry pin contains the closed #8035 fix and emits no
   skipped-forwarding warning during preload.
7. The production App Route pipeline runs without a known-response shortcut,
   compatibility shim, or fixture-only rewrite.
8. Development source, production bundle, and incremental rebuild paths all
   use the same strict application ABI.

### Fixture progression

1. Minimal TypeScript Buffer handler: validates hosting and ABI mechanics.
2. Framework-shaped route: validates `NextRequest` and `NextResponse` APIs.
3. Production Next App Route: validates webpack modules, async stores, response
   extraction, and the real route-module call.
4. Small production standalone application: enables equivalent Perry/Node
   measurement.
5. One, ten, and one hundred distinct builds: validates fleet behavior.

### Acceptance

The Perry application must execute the same route source and framework pipeline
as the Node production build and return the same validated status, headers, and
body for the test corpus. A lower-level approximation remains useful for
profiling but cannot be labeled a Next.js comparison.

## Workstream 3: per-application memory density

This is the largest independent obstacle to an unconditional Perry verdict.
The native 100-tiny-app result is much better than 100 independent Node
processes (99.3 versus 1,407.5 MiB warm PSS), but it is above one Node process
multiplexing 100 logical apps (47.7 MiB). Perry wins the same consolidated
comparison at one and ten apps, so the measured problem is its incremental
per-app private state rather than the process-wide provider floor.

### Completed controls and measurements

- Added per-application live and reserved Perry arena statistics at ready and
  warm lifecycle points, an authenticated per-deployment memory endpoint, and
  bounded-cardinality live/reserved Prometheus gauges.
- Made executor stack size explicit (1 MiB default, 256 KiB minimum).
- Replaced the unbounded executor command channel with an explicitly bounded
  queue (256 entries by default) and immediate overload rejection.
- Added deterministic Perry thread-local teardown after application shutdown.
- Made provider allocator and arena-block size explicit package identity.
- Compared 1 MiB/mimalloc, 256 KiB/mimalloc, 256 KiB/system, and 128
  KiB/system provider profiles. Moving to the system allocator produced the
  material RSS reduction; reducing arenas from 256 to 128 KiB reduced reserved
  address space but only modestly reduced RSS.
- Added a repeated static-only replacement path that reuses a healthy,
  byte-identical initialized runtime. The 250-cycle daemon proof had no second
  `dlopen`, no compiler invocation, no traffic error, no thread trend, and only
  one additional descriptor while retaining exactly three packages.
- Isolated the remaining replacement/traffic RSS trend to Perry arena data. A
  100-cycle sample increased live bytes by about 22 KiB/cycle, and a direct
  50,000-request probe increased live bytes by exactly 152 bytes/request.
- Verified from the pinned Perry source that host ABI Buffers are allocated
  TENURED in the old generation. Minor collection cannot reclaim them, and no
  automatic full collection occurred during the 50,000-request probe.
- Rejected host-triggered full GC as a Coop workaround after it corrupted the
  next tiny-app ABI response. An ignored promotion test,
  `host_buffer_churn_is_reclaimed_by_perry`, now preserves the reproducer and
  must pass on any proposed Perry revision.
- Added same-sample Linux `smaps_rollup` PSS and `Private_Dirty` accounting to
  the Perry, consolidated/isolated Node, common server, compilation, and
  replacement-soak harnesses. The soak now records a private-dirty slope as a
  separate retained-memory gate rather than inferring it from aggregate RSS.
- Retained the first native 1/10/100 sample: Perry warm PSS was 23.6, 31.0, and
  99.3 MiB; consolidated Node was 46.7, 46.8, and 47.7 MiB. This identifies the
  100-app per-deployment slope as the next density target.

### Remaining tasks

- Fix Perry's host-ABI Buffer rooting/reclamation or full-GC pacing so dead
  request and response Buffers are reclaimed automatically without response
  corruption. Make the 50,000-request promotion test green and add a positive
  full-GC/root-safety regression in Perry itself.
- Turn the full-daemon arena live/reserved slope into a hard benchmark failure;
  rerun 250+ replacement cycles and a request-only sustained-traffic soak after
  the Perry fix, requiring a statistically flat latter-half retained-memory
  slope.
- Expand the retained two-trial RSS/PSS/private-dirty/cgroup matrix to five
  trials on the production host and selected Node version; add a provider-only
  row whose cgroup owns provider first-touch so shared-page charging is explicit.
- Finish attributing incremental memory beyond measured stacks, queues, and GC
  arenas: Perry TLS, module roots, event-loop state, application globals,
  allocator fragmentation, and private dirty mappings.
- Record memory immediately after thread spawn, GC initialization, `dlopen`,
  module initialization, first request, and fixed workload.
- Validate the 128 KiB arena profile under realistic allocation-heavy apps and
  tune its fresh-block threshold before treating it as a production default.
- Remove allocations caused by one-time request framing or warmup that can be
  safely reused.
- Determine whether Perry can expose explicit runtime contexts instead of
  relying on thread-local singleton state.
- If explicit contexts become possible, evaluate a bounded executor pool while
  preserving independent application state and request serialization rules.

### Acceptance

For equivalent warmed applications, Perry must use less aggregate PSS/cgroup
memory than process-per-app Node. The final “Perry wins everywhere” verdict also
requires beating a defensible consolidated Node topology, not only a deliberately
fragmented baseline. All advertised warm memory must include every application
initialized and ready to serve without first-request allocation work being
misclassified as startup savings.

## Workstream 4: startup and activation scaling

### Completed

- Providers and applications use eager loading/binding, and module
  initialization completes before listener/router publication.
- Provider validation/load, app manifest/integrity/boundary/load/symbol binding,
  module initialization, and sampled dispatch phases emit timing data.
- The resource harness retains one artifact set and reports fresh image
  activation separately from ordinary process restart.
- The native Linux harness has retained 1/10/100 fresh/restart, ready/warm,
  CPU, PSS, private-dirty, and cgroup evidence over immutable packages.
- The common Worker oracle now has five validated Linux trials for Perry,
  plain Node, and celld under fail-closed cgroup accounting. The harness resets
  only its generated active pointer before priming, so a stale deployment state
  cannot silently select an earlier package.
- Portable provider verification hashes runtime and stdlib concurrently. The
  root-owned immutable Linux package mode reduces provider manifest validation
  from hundreds of milliseconds to hundredths of a millisecond without making
  writable provider bytes trusted; the measured Perry usable restart is now
  55.6 ms rather than the original 565.7 ms full-hash baseline.
- macOS defaults to serial preload after the bounded-parallel control measured
  slower under dyld; other platforms retain a bounded parallel default.
- Replacements compile, validate, publish immutable bytes, preload, initialize,
  run one to 64 bounded app-defined HTTP warmup/health requests, and swap only
  after the expected status and optional body digest pass. Failure drains the
  unpublished generation; exact healthy package reloads are true no-ops.
- Request routing reads a lock-free atomic immutable router snapshot. It borrows
  method/path/query/host during lookup, carries immutable HTTP limits with the
  matched generation, omits unused handler/tool clones, and uses cached
  admission and request metric handles instead of registry lookup and label
  allocation on every call.
- A sampled phase trace puts strict COOP encode plus decode at roughly 1.4 us
  and the Perry invocation at roughly 42 us for the tiny Worker oracle. This
  makes Perry execution and Buffer lifetime the next targets, not another
  routing-wrapper micro-optimization. Shared-VM post-change CPU runs varied too
  much with host load to replace the retained five-trial baseline.

### Remaining tasks

- Preserve and publish complete structured phase timings for provider manifest
  validation/hash/load, symbol probes, app integrity validation, app `dlopen`,
  module init, thread readiness, listener publication, and first validated
  request.
- Expand the retained 1/10/100 Linux restart sample from two to five trials on
  the production host and selected Node version.
- Profile app SHA-256 verification for 100 realistic application images.
- Design a deployment-time verification cache or immutable artifact contract
  that avoids rehashing large unchanged apps on every restart without weakening
  boundary enforcement.
- Compare serial and bounded-parallel preload on Linux using the retained
  immutable artifact set; the current Linux sample used the default bound and
  is not an A/B loader-concurrency result.
- Profile thread creation and runtime initialization independently of `dlopen`.
- Treat macOS fresh-image activation as explicit deployment work and investigate
  whether signing, install-name mutation, artifact layout, or staging changes
  can reduce it.
- Add activation-probe duration to the existing Linux CPU/PSS readiness
  evidence and repeat on the isolated production host.

### Acceptance

- Listener-ready means all advertised applications are genuinely invokable.
- First request performs no loader, binder, module, or compiler work.
- Restart is measured over deployed immutable artifacts.
- Fresh deployment activation is reported separately rather than hidden.
- Perry must beat the equivalent isolated Node topology and become competitive
  with consolidated Node startup at the target application count.

## Workstream 5: compilation and deployment economics

Compilation is intentionally outside request startup, but it still determines
whether on-demand deployment is operationally viable.

### Completed

- Cache final compiler outputs by an exact digest of sorted source/config/lock
  paths and bytes, the dereferenced installed dependency tree, serialized
  deployment configuration, provider/compiler/invocation identity, target, CPU
  baseline, and ABI manifest rather than filesystem timestamps.
- Copy all supported local application inputs into a private staging snapshot,
  compile from that snapshot, reject local source symlinks, and re-hash the
  mutable deployment before and after compiler/package assembly so a concurrent
  source change discards the result rather than publishing mixed bytes.
- Compile into a unique same-filesystem staging directory and clean it on
  every compiler, validation, integrity, or publication failure.
- Hash and boundary-check the staged application before it can become visible.
- Publish the library and adjacent ABI/integrity manifest together by an
  atomic directory rename to
  `compiled/<deployment>/<package-sha256>/app.{dylib,so}`.
- Reuse an identical immutable package without rewriting it; reject a digest
  collision or corrupted existing package.
- Remove the mutable legacy-library fallback. Source-less restoration uses
  only published content-addressed packages that pass current ABI and integrity
  verification.
- Bundle the exact deployment configuration with the app and manifest, and
  bind all three byte sequences into the immutable package identity.
- Snapshot every configured static root into the package, record sorted
  path/size/SHA-256 entries, rewrite the packaged configuration to those
  immutable roots, and enforce file-count/byte limits plus symlink and special-
  file rejection. Rollback and restart therefore serve the old static bytes,
  not whatever currently exists in the mutable deployment directory.
- Persist active and previous package identities in an atomically replaced
  deployment-state record.
- Re-verify the complete package and eagerly initialize it before an
  authenticated operator rollback can publish it.
- Retain packages by count and age while pinning the active, rollback, and live
  runtime identities; collection renames packages into a private trash area
  before deletion and refuses symlinked or malformed trees.
- Serialize activation, rollback, and collection per deployment and reconcile
  abandoned staging/trash/state-temporary entries on startup without following
  symlinks. PID-owned compiler staging is removed immediately when its owner is
  dead while a live owner's tree remains protected by the configured grace.
- Bound compiler concurrency independently of preload concurrency, expose queue
  wait/cache/outcome/duration/peak-RSS metrics, and enforce wall-clock and
  aggregate process-group RSS budgets. Timeout or memory exhaustion kills the
  whole compiler process group; stdout/stderr are drained concurrently while
  retaining only configured bounded diagnostics. Run Perry behind a private
  process-group guard that watches the exact daemon parent, so daemon SIGKILL
  cannot orphan the compiler or its descendants when `kill_on_drop` cannot run.
- Clear the ambient compiler environment, propagate only an audited allowlist,
  disable host-declared codegen hooks, pin the CPU baseline, hash the exact
  semantic argv/tool/provider/environment contract, and retain that digest in
  every app manifest.
- Hash and dereference the complete installed `node_modules` closure into the
  private source snapshot, including linked workspace packages and package
  manifests, while excluding only Perry's machine-local cache. Hard file and
  byte limits reject pathological dependency trees before compiler spawn.
- Place Perry's content-keyed object cache in a Coop-owned namespace
  keyed by the compiler contract so safe frontend/object products are reused
  across deployments. Record Perry object hit/miss results, Coop package and
  compiled-image reuse, and identity/snapshot/compiler/validation/package/
  publication phase success/failure durations. Perry's current dylib report
  does not expose an independent link-cache result.
- Provide `coop build <deployment>...` and `coop build --all` for explicit
  deploy-pipeline prebuild. It verifies and publishes immutable packages but
  never activates them or changes active deployment state.
- Exercise the complete path with a real compiler/daemon integration: prebuild,
  startup cache hit, code/config/static replacement, authenticated rollback,
  exact restart restoration without compilation, failed-compile preservation,
  artifact status/metrics, and direct HTTP/cron/queue ABI calls.
- Retain a resident-daemon harness covering cold activation, exact no-op,
  one-module, dependency-only, static-only, and rollback changes. The loaded
  macOS directional sample proves compiler spawn/cache behavior and records
  phase/CPU/RSS evidence in `benchmarks/results/`; comparison-quality medians
  and PSS remain an isolated-Linux gate.

### Remaining tasks

- When Perry exposes its resolved module graph, narrow the deliberately
  conservative full installed-dependency snapshot to the actually reachable
  closure without weakening identity. Today an unused package change causes a
  safe extra rebuild.
- Keep Perry's existing per-module object cache as the frontend reuse boundary
  and Coop's verified compiled-image reuse as the no-code-change boundary; add
  another split only if controlled measurements show a remaining bottleneck.
- Run the retained compilation matrix on the isolated Linux host for stable
  medians, PSS/cgroup memory, and phase-specific CPU/peak RSS.
- Repeat the locally passing compiler-start, validated-staging,
  post-probe/pre-state activation, package/state, and trash real-SIGKILL matrix
  on native Linux, then retain the long replacement/collection soak.
- Extend rollback/retention coverage with corrupt-artifact, busy-drain,
  concurrent-watcher, and long collection-soak cases.

### Acceptance

A small incremental change must have bounded wall time and peak memory suitable
for server automation. One failed or memory-heavy build may not starve serving
applications or block unrelated deployments.

## Workstream 6: ABI completion

### HTTP

HTTP ABI v2 is the current baseline: a versioned `COOP` Buffer carries method,
URL fields, duplicate headers, address metadata, and raw body bytes in both
directions.

Wall deadlines, admission, and buffered request/header/response byte limits are
implemented. A timed-out in-process call retains its permit until native work
really ends; an uncertain worker transport is poisoned and replaced. Remaining
HTTP work after basic Next correctness:

- define cooperative cancellation and client-disconnect propagation while
  preserving the honest limitation that synchronous in-process native code
  cannot be forcibly preempted;
- decide when buffered bodies are sufficient and when a versioned streaming ABI
  is required;
- define streaming backpressure if a new ABI is justified;
- fuzz frame parsing beyond the existing strict malformed/trailing/size tests;
- benchmark large bodies and duplicate headers, not only tiny JSON responses.

### Cron and queue

Completed at the application and durable-service boundaries:

- versioned binary cron request/response and queue request/response frames;
- exact cron and queue handler entries in the application manifest;
- raw in-process queue payloads with explicit ack, nack, and dead-letter
  dispositions;
- sync/async invocation plus strict malformed/trailing-frame tests;
- compilation fixtures that exercise HTTP, cron, and queue in one app-only
  library with exactly the configured exports.
- daemon and isolated-worker dispatch clients for cron and queue, retaining raw
  queue bytes until the worker socket boundary;
- cron schedule validation before replacement, activation only after the new
  runtime is published, and task cancellation before old-runtime drain;
- a provider-owned enqueue import that obtains the active deployment through an
  opaque host context, so application code neither owns queue SQL nor relies on
  a process-global database/search-path identity;
- public `queue.send` and `queue.sendRaw` producer calls; the raw form crosses
  the application/provider ABI as Buffer pointer plus length and is proven to
  retain arbitrary bytes exactly in Postgres without JSON or Base64;
- a Coop-owned Postgres migration and pooled store for messages and dead
  letters, including immutable message/deployment/queue identity, raw `BYTEA`
  payloads, attempts, availability, lease owner/token/expiry, timestamps, and
  bounded last-error data;
- atomic `FOR UPDATE SKIP LOCKED` claims with expiring leases, exact-token
  acknowledgement, deterministic bounded exponential retry/backoff, and atomic
  dead-letter movement for explicit or exhausted deliveries;
- deployment queue consumers that reserve application admission before
  claiming, honor queue concurrency and deadlines, stop the old generation
  before activating the replacement generation, and release a claim without
  consuming an attempt if delivery never began;
- queue policy for payload size, enqueue delay, visibility timeout, maximum
  attempts, retry bounds, and DLQ retention;
- bounded-cardinality metrics for queue depth, visible messages, oldest age,
  leases, claims, deliveries/dispositions/duration, retries, expired leases,
  store errors, dead letters, and pruning;
- a real-Postgres store integration covering lease exclusion and recovery,
  retry timing, exhaustion/DLQ, parallel unique claims, binary bytes, tenant
  isolation, and pruning;
- a real-daemon integration that compiles and loads an actual Perry app,
  enqueues through the provider callback, commits to Postgres, claims and
  dispatches through the raw queue ABI, and acknowledges in trusted in-process,
  dedicated-worker, and sharded modes;
- the sharded Postgres branch verifies provider-callback `queue.send` and
  `queue.sendRaw`, exact raw-byte delivery to the intended runtime ID,
  acknowledgement, shared-worker identity, and shard metrics against a
  disposable real database;
- restart over the exact retained package, authenticated hot replacement to a
  new package identity, replacement-generation delivery, and expired-lease
  recovery without losing durable ownership;
- full-daemon explicit and exhausted-DLQ behavior, rollback-generation
  leadership, and a deliberately killed in-flight delivery whose expired
  lease is recovered after daemon restart;
- forced termination of the daemon's PostgreSQL backends followed by an
  in-place producer and consumer recovery through the connection pool;
- complete PostgreSQL stop/start followed by producer and consumer recovery
  without restarting the daemon; enqueues fail closed while the database is
  unavailable, and the queue consumer uses bounded exponential retry with
  power-of-two warning sampling to avoid an outage compute/log storm;
- one-second pool health snapshots expose maximum/open/available connections,
  waiting borrowers, and checked-out utilization; the dashboard and alert pack
  surface sustained saturation without deployment- or message-level labels;
- authenticated, confirmation-gated DLQ replay and purge plus paginated,
  metadata-only inspection that does not expose stored payload bytes; operator
  actions are logged and counted with bounded-cardinality metrics;
- fail-closed activation for queue-configured deployments without a durable
  store, with an explicit ABI-only escape hatch for narrow tooling tests.

The delivery contract is **at-least-once**. The stable delivery ID is passed to
the application so handlers can make side effects idempotent; exactly-once
execution is not claimed.

Remaining at the production lifecycle boundary:

- run concurrency and long-duration queue soaks while sampling Postgres pool
  use, consumer/background-task counts, leases, PSS, and shutdown/drain time;
- repeat the now-passing full PostgreSQL stop/start gate on the Linux production
  runner and retain recovery timing plus pool/resource telemetry.

Cron policy is explicit: UTC is the only accepted timezone; overlap and late
wakeups default to `skip`, with opt-in `allow` and `run_once`; daemon downtime
is intentionally not replayed; ordinary deployment deadlines/admission bound
every fire; replacement has one active scheduler generation and joins already
dispatched fires. Dispatch, overlap/late skips, lateness, duration, overload,
timeout, and failures are metricized. The long durable queue soak and native-
Linux repetition remain the background-work production evidence gaps.

### Worker isolation transport

Worker mode retains process isolation and a Unix-socket protocol. Optimize or
replace its JSON framing only after measuring it; in-process hosting remains the
primary density path.

## Workstream 7: reliability, isolation, and multitenancy

One native process hosting 100 apps changes the blast radius. A thread isolates
Perry state but does not contain a segmentation fault, allocator corruption, or
host-process abort.

### Completed baseline

- Executor command queues are bounded and reject overload rather than growing
  without limit.
- HTTP, cron, and queue share one per-deployment admission semaphore. The
  configured maximum is capped at the command-queue capacity, and acquisition
  is non-waiting so overload cannot create another unbounded queue.
- Raw HTTP request/header/response and queue payload limits are configured per
  deployment. Exact COOP frame size is checked without allocating another frame;
  the global 16-MiB ABI ceiling still applies.
- HTTP overload returns stable `503` plus `Retry-After` and `x-coop-error`;
  request body/header limits return `413`/`431`, an oversized app response
  returns `502`, and a wall deadline returns `504`.
- Wall deadlines retain the admission permit until the underlying native work
  really completes. Worker transports with uncertain response state are
  permanently poisoned, never reused, and replaced using generation-unique
  socket paths; the health watchdog checks poison state every second.
- The real daemon integration drives an infinite synchronous handler through a
  250-ms deadline and requires a stable `504 deadline_exceeded`. In dedicated
  mode it proves a new healthy PID is published, immediately SIGKILLs that
  successor, and requires a second healthy generation. In sharded mode it
  proves the timeout retires the complete two-application failure domain, both
  residents return in one new shard PID, and a subsequent SIGKILL again
  reconstructs both residents together.
- Restart de-duplication is scoped to the failed client generation rather than
  only the deployment name, so a freshly published successor can fail while a
  prior replacement task is still unwinding. A poisoned dedicated generation
  is killed immediately when protocol shutdown cannot be trusted; a poisoned
  shard is left to the domain watchdog so no single-app reload can publish into
  a failure domain that is about to be retired.
- The daemon passes the exact generation socket path to each worker and waits
  for a successful protocol handshake, not merely the socket inode. Graceful
  shutdown cancels the worker listener and removes that generation's socket,
  so a normal replacement does not wait for the kill timeout.
- Replacement stops old background tasks and drains the old executor after the
  routing swap. Worker shutdown has bounded grace followed by process kill;
  in-process shutdown is timed out after 15 seconds and logs that native code
  cannot be forcibly cancelled safely.
- Worker mode has an RSS supervisor and configurable worker RSS ceiling.
- Deployment configuration now has an immutable `[isolation]` policy:
  `trusted` forces in-process execution, `sharded` selects a bounded shared
  worker, `dedicated` forces one supervised worker, and `inherit` resolves the
  box-wide default. A policy change cannot reuse a byte-identical runtime
  across execution modes. Admin memory status, structured activation logs, and
  numeric Prometheus gauges expose requested and effective isolation without
  stale mutable class labels; Linux worker status reports the worker PID and
  that process's RSS.
- Dedicated Linux worker generations get unique cgroup-v2 directories with
  `memory.max`, swap disabled, group OOM kill, `cpu.max`, and `pids.max`. The
  worker self-attaches before provider/application loading. `auto` mode records
  fallback to the RSS supervisor when no delegated hierarchy exists;
  `required` fails activation closed. Current/peak memory, CPU, PIDs, and OOM
  events are exposed through admin status and metrics.
- Sharded execution is a real multi-application worker mode, not an alias for
  dedicated workers. Deployment names have a stable SHA-256 preferred slot and
  deterministically probe the remaining lazily started slots when that shard is
  full, so hash skew cannot strand bounded fleet capacity. Each process loads
  the provider pair once, dynamically loads exact runtime generations, caps the
  number of distinct resident applications, and permits overlapping old/new
  generations of the same application during activation and drain.
- The daemon-worker protocol has generation-scoped load and unload operations;
  every HTTP, cron, and queue dispatch carries the exact runtime identity.
  Unloading one application removes only that generation and preserves sibling
  deployments. Load is idempotent only for the same runtime ID and byte-exact
  deployment specification; a conflicting reuse fails closed. Lost control
  responses are retried with the same identity under a fixed timeout. A
  definitive rejection preserves healthy siblings, while an outcome that
  remains uncertain retires the whole shard so an unreachable native executor
  cannot remain resident. A load failure does not publish the new generation.
- A shard has aggregate memory, CPU, and PID cgroup limits. Its watchdog treats
  child exit, cgroup OOM, RSS-limit action, or poisoned transport as a failure
  of the complete domain, poisons every resident client, terminates the old
  process, and reloads all affected deployments into a new generation with the
  existing bounded restart backoff.
- The real auto-compile integration test also loads two separately compiled
  deployments into one shard, verifies their shared PID and slot, exercises
  the timeout and repeated-crash sequence above, validates both routes, then
  moves one deployment to trusted execution and proves the sibling remains
  served by the shard. It then moves the final deployment out, proves the idle
  shard remains owned by the daemon, SIGKILLs the daemon, and requires the
  shard to exit rather than survive as an orphan.
- Every daemon-spawned dedicated worker and shard receives the daemon's exact
  PID. Linux also installs `PR_SET_PDEATHSIG`; a portable Unix parent-identity
  watcher covers macOS and the initialization race. Manual workers may omit the
  binding, but supervised generations cannot outlive a crashed owner.
- The same real integration uses a one-second cron schedule, snapshots the
  successful-invocation counter after shard publication, and requires a later
  success so cron dispatch is proven through an exact shard runtime ID rather
  than only by direct host calls.
- Focused tests prove that replacement generations do not consume another
  logical-app slot, exact load retries are idempotent, runtime-ID collisions and
  invalid specifications fail closed, lost responses retry the exact request,
  exhausted retries are classified as uncertain, and the default four-by-25
  placement probes around hash skew to fit exactly 100 apps while rejecting the
  101st.
- The supervisor reaps and replaces an idle crashed worker without waiting for
  a request. The real daemon test covers a deadline-poisoned generation and an
  immediately killed successor in dedicated and sharded modes, observes new
  PIDs and healthy responses after both failures, then transitions back to
  trusted in-process mode. Failed restart attempts use a bounded 1–64 second
  exponential backoff.
- The opt-in real-library lifecycle soak completed 250 measured
  load/dispatch/shutdown cycles after five warmups in 1.44 seconds. Process
  threads stayed 3→3 (peak 3), descriptors 10→10 (peak 10), and RSS moved
  17,024→19,840 KiB. The configurable test enforces thread, FD, and a 64-MiB
  retained-RSS ceiling.
- The full-daemon replacement soak completed 250 byte-identical static-only
  replacements during 21,740 validated traffic requests. It reused the live
  runtime on all 250 changes, never invoked the compiler or loaded a second app
  image, retained three packages, and kept threads flat. The run deliberately
  remains a failed production gate because RSS and Perry live-arena bytes have
  a positive retained slope.
- Per-invocation memory remains descriptive because current Perry applications
  do not have isolated linear heaps. Worker RSS and admission/byte/deadline
  controls are enforceable to the limits documented here.

### Remaining tasks

- Repeat the passing dedicated and two-app shard hard-timeout/repeated-crash
  matrix on native Linux with delegated cgroup v2, then add memory OOM, PID
  exhaustion, CPU throttling, poisoned framing, and idle-exit injection. Prove
  that the advertised failure domain and only that domain is recreated, and
  retain recovery time plus cgroup event evidence.
- Run shard saturation and repeated-crash soaks with several slots, more apps
  than one slot can accept, continuous mixed HTTP/cron/queue traffic, hot
  replacements, and bounded restart backoff. Retain availability, tail latency,
  cgroup peak, tasks, descriptors, sockets, images, and recovery time.
- Define cancellation separately for cooperative async Perry work and
  synchronous native calls. In-process mode cannot hard-kill the latter; worker
  and shard modes may terminate and recreate the containing process after grace.
- Retain a green privileged/delegated Linux proof for the implemented
  multi-app shard cgroup controller. Per-invocation memory/CPU limits remain
  advisory within a shared shard unless Perry exposes accounting/preemption
  hooks; only the aggregate shard budget is enforceable today.
- Add response-size, open-file, outbound-fetch, database, and storage policies
  where the platform permits enforcement. Reject configuration that promises a
  guarantee unavailable in the selected isolation mode.
- After Perry's host-Buffer GC fix, repeat the implemented full-daemon
  sustained-traffic replacement soak with loaded-image, arena, route,
  background-task, Postgres-connection, package, PSS, and cgroup sampling. The
  correctness assertions already pass; closure requires a flat retained-
  resource slope.
- Extend recovery proof beyond the now-covered infinite loop and external
  process kill to executor panic, rejected promises, native crashes, and
  provider initialization failure.
- Choose and document the production class/default for each trust tier,
  including which isolation promises are unavailable in trusted mode and which
  sibling disruption is inherent in sharded mode.

### Acceptance

The production mode must make its crash and resource-isolation tradeoff explicit.
A density benchmark cannot silently compare trusted in-process Perry with a
stronger process-isolated Node topology without reporting that distinction.

## Workstream 8: observability

### Implemented baseline

- Prometheus deployment-count gauge is updated on every router rebuild.
- Routed application HTTP calls record deployment/method/status counters and
  end-to-end dispatch latency, with an integration assertion against the live
  metrics endpoint.
- Deployment admission exports current in-flight work plus fixed-reason
  rejection and timeout counters. The real auto-compile HTTP integration proves
  a listener-side body rejection is counted without application invocation.
- In-process executor queue depth and capacity are sampled once per second by
  the health supervisor. Dedicated and sharded worker connections additionally
  expose cancellation-safe transport backlog/in-flight gauges, handoff wait,
  protocol round-trip, graceful drain histograms, complete framed byte volume,
  fixed poison causes, and future-drop cancellation phase with bounded labels.
  Cancelling before socket acquisition leaves the connection reusable;
  cancelling an active exchange poisons it immediately because its framing is
  uncertain, and a focused regression proves the connection is never reused.
- Compiler queue wait, cache/outcome, wall duration, and sampled process-group
  peak RSS are recorded with bounded result labels. Compiler output retention
  is bounded independently from metric collection.
- Artifact package count/retained bytes, collection outcomes and removed
  package totals, startup reconciliation outcomes, and rollback outcomes are
  exported. The admin status endpoint reports active/previous packages and
  retained inventory.
- Provider validation/load, application manifest/boundary/`dlopen`/symbol
  binding, module initialization, sampled dispatch phases, and ready/warm RSS
  already emit structured phase data off the request hot path.
- Deployment memory status reports execution mode, process RSS where
  applicable, Perry arena live/reserved bytes, and executor queue
  depth/capacity. Arena gauges are also refreshed by the periodic watchdog.
- Byte-identical activation reuse has its own counter, allowing the lifecycle
  harness to assert that a replacement neither compiled nor loaded a second
  native application image.
- Effective/inherited isolation, cgroup preparation/fallback, current/peak
  cgroup memory, CPU use, task count, OOM/OOM-kill events, restart outcome and
  bounded backoff are exported. Admin memory status includes the worker PID and
  cgroup sample; the real daemon suite proves idle killed-worker replacement.
- Sharded execution exports per-deployment sharded state, stable slot and live
  generation gauges, shard process start/failure counters, and resident-
  deployment count. Admin memory status exposes the shared PID, slot,
  generation, cgroup sample, and effective execution mode for each resident
  deployment.
- Durable queues export depth, visible count, oldest age, active leases,
  claims/expired leases, delivery outcome and duration, retry/deferral/store
  errors, DLQ totals, pruning, and shared-pool capacity/availability/waiters/
  utilization without message-ID labels.
- [`ops/prometheus/coop-rules.yml`](ops/prometheus/coop-rules.yml) ships
  recording rules and alerts for the metric surface, HTTP errors, executor
  saturation, worker-transport backlog, rejection/deadline events, activation
  health, crash loops, shard failure, worker OOM kills, durable-queue
  age/DLQ/store failure, rollback, and artifact collection.
  [`ops/grafana/coop-overview.json`](ops/grafana/coop-overview.json)
  provides the corresponding raw-metric fleet dashboard. The repository
  validator parses the dashboard, rejects any `coop_*` reference not emitted
  by `coop-daemon`, and requires a unique matching
  [`ops/RUNBOOK.md`](ops/RUNBOOK.md) procedure for every alert. Prometheus
  3.13.2 accepts all 22 rules and its native synthetic suite passes ratio,
  volume, hold-duration, increase, label, and annotation cases. Receiver names
  and credentials remain environment-owned; live Prometheus/Grafana validation
  is still required.
- [`ops/metric-label-policy.json`](ops/metric-label-policy.json) accounts for
  all 18 emitted label keys and rejects high-cardinality request/message IDs,
  paths, hosts, error strings, PIDs, and package/runtime identities. Public
  extension HTTP methods normalize to `OTHER`. A release-only 500,000-iteration
  probe measures the two request-path updates with 10,000 warmups and a 1-us
  regression ceiling. On the loaded macOS development host, caching registered
  metric handles reduced this direct cost from 997.7 ns to 140.2 ns per request
  (about 86%) by removing repeated registry lookup and label allocation.
- [`ops/smoke-live.sh`](ops/smoke-live.sh) provisions the repository artifacts
  into disposable live services and verifies them over their APIs. A local run
  against Prometheus 3.13.2 and Grafana 13.1.3 loaded all 22 rules, scraped and
  queried the finite Coop fixture, provisioned dashboard UID
  `coop-server-overview`, and received a healthy response for datasource UID
  `coop-prometheus`. The same smoke remains a production-Linux repetition
  gate; fleet Alertmanager receivers and credentials are environment-owned.

### Remaining required metrics

- provider manifest/hash/load/symbol-probe duration and identity;
- app source snapshot, cache lookup, manifest/hash/boundary audit, `dlopen`,
  symbol binding, executor readiness, module init, warmup, and health duration;
- ready, warm, and post-workload RSS/PSS, private dirty, thread count, open
  descriptors, and mapped images (per-worker cgroup current/peak, scenario-wide
  benchmark cgroup current/peak, Linux benchmark private dirty, and Perry arena
  live/reserved are implemented; fleet descriptor/image capture remains);
- HTTP/cron/queue dispatch count, errors, panics, timeouts, cancellations,
  status/disposition, request bytes, and response bytes;
- ABI encode, admission wait, handoff wait, application execution, decode, and
  end-to-end time/CPU;
- compiler CPU time, output size, bounded validation-failure class, and staging
  cleanup outcome (queue wait, wall time, peak RSS, cache, and outcome already
  exist);
- deployment snapshot, validation, activation, warmup, swap, drain, failure,
  quarantine, rollback, and recovery duration/count;
- queue reconnect outcomes and cron fired/skipped/late/error totals (pool
  saturation, replay operator, and core queue depth/age/lease/retry/DLQ metrics
  are implemented);
- reload count, activation/rollback duration, and mapped-image count (package
  count, retained bytes, collector outcome, reconciliation, and rollback
  counters already exist);
- worker/shard unavailable time and recovery duration (worker restart
  outcome/reason, bounded backoff, cgroup OOM events, RSS-limit scheduling
  logs, shard starts/failures/residency, and per-deployment shard identity are
  implemented; explicit kill/RSS-limit action counters and availability SLI
  remain).

Sampling and logging must stay off the hot request path unless explicitly part
of the measurement. Metrics should identify deployment and phase without
creating unbounded label cardinality.

Structured deployment events should include a stable deployment name,
generation/package identity, provider identity, execution mode, phase, elapsed
time, and outcome. Request IDs, queue message IDs, source paths, hostnames, and
error strings must not become Prometheus labels. The shipped dashboard and
alerts cover activation failures, crash loops, queue age/DLQ state, overload,
deadline violations, OOM-kill state, and failed rollback/collection. Synthetic
rule behavior, one-to-one repository runbooks, the complete bounded label-key
inventory, public-method normalization, and direct hot-path cost are now
locally proven, including live Prometheus/Grafana provisioning. Remaining
operations proof is environment receiver configuration and repetition of the
provisioning/cost gates on the production Linux runner.

## Workstream 9: benchmark and verdict protocol

### Server shapes

| Runtime | Shapes to measure |
|---|---|
| Perry | One process with 1, 10, and 100 eager app libraries |
| Node consolidated | One process with 1, 10, and 100 logical applications |
| Node isolated | 1, 10, and 100 processes with one application each |
| celld | Equivalent Worker/cell fleet at 1, 10, and 100 where supported |

### Workloads

1. Tiny Buffer handler: isolates host architecture and per-app overhead.
2. Deterministic compute route: validates CPU accounting and response equality.
3. Equivalent Worker route: enables a defensible celld mechanism comparison.
4. Full production Next App Route: compares Perry with production Node/Next
   after every compatibility gate passes.
5. Mixed workload: small bodies, larger bodies, async I/O, and an incremental
   deployment while traffic continues.

### Measurements

- artifact and provider bytes;
- cold and incremental compile time and max RSS;
- fresh deployment activation;
- restart over retained artifacts;
- listener-ready and first-request time;
- usable cold start;
- ready, warm, and post-workload RSS, Linux PSS, private-dirty, and cgroup
  memory;
- startup CPU and server CPU per request;
- throughput and latency p50/p95/p99;
- errors, response validation, and unexpected first-request work.

### Controls

- isolated Linux host or dedicated VM/cgroup;
- fixed CPU model, kernel, governor, power state, and toolchain;
- same request count, concurrency, keep-alive behavior, and response validation;
- no unrelated compilation or workload;
- at least five process trials after a separate fresh-activation trial;
- retained exact artifacts across restart trials;
- raw per-trial output checked in with environment metadata;
- report RSS and PSS/cgroup memory together for multi-process comparisons.

celld's listener may appear ready before a cell is activated. Its meaningful
cold metric is listener start plus the first validated request. celld is not a
Next.js host, so it must be labeled as an equivalent Worker workload rather than
presented as a full Next comparison.

### Verdict scorecard

| Claim | Current verdict | What changes it to “Perry” |
|---|---|---|
| Separate shared providers and app-only libraries work | Perry | Retain boundary/integrity gates on macOS and Linux. |
| No request-path cold initialization | Perry | Keep eager activation and prove phase traces contain no deferred work. |
| Tiny apps versus process-per-app Node memory | Perry on native Linux diagnostic evidence | Repeat for five trials under the selected production isolation class. |
| Tiny apps versus consolidated Node memory | Perry at 1/10; Node at 100 | Reduce Perry's measured per-app private/TLS/thread state below the 100-app consolidated control and repeat for five trials. |
| Full Next.js correctness | Not yet comparable | Close #8040 with the exact black-box oracle and no fallback. |
| Full Next.js warm memory/CPU | Not yet comparable | Run the green production Next pipeline with equal work and beat Node on Linux. |
| Tiny-app restart versus consolidated Node | Node at 1/10/100 | Profile and reduce eager provider/app startup while keeping first-request work at zero. |
| Tiny-app restart versus process-isolated Node | Perry at 100 | Repeat for five isolated Linux trials over retained artifacts. |
| Fresh deployment activation | Needs improvement/evidence | Bound compile/validation/activation and report it separately from restart. |
| Equivalent Worker steady-state CPU | Node: 87.5 us; celld: 117.0 us; Perry: 118.5 us | Reduce Perry invocation/runtime overhead below both controls and repeat the identical validated oracle on the production host with randomized or interleaved order. |
| Equivalent Worker usable cold start | celld: 23.0 ms; Perry: 55.6 ms; Node: 147.8 ms | Keep Perry eagerly usable while reducing provider/app/process startup below celld's listener-plus-lazy-first-cell boundary. |
| Production reliability/isolation | Undecided | Select trust classes and pass limits, crash, drain, rollback, queue, and soak gates. |

No aggregate winner is published while any applicable correctness, isolation,
or reproducibility row is “not comparable” or “undecided.” The final report
must preserve losing rows rather than collapsing them into a geometric score.

## Success gates

### Architecture gate

- runtime and stdlib are separate provider files;
- apps contain no provider implementation code;
- providers load once and apps bind eagerly;
- no compile, process spawn, `dlopen`, binding, or module init occurs on request.

### Correctness gate

- full production Next pipeline passes the fixture corpus without shortcuts;
- status, duplicate headers, and body bytes match Node;
- sync, async, error, timeout, and reload paths pass;
- no legacy application ABI or manifestless loading remains.

### Efficiency gate

For equivalent work at the target deployment count, Perry must have:

- lower warm PSS/cgroup memory;
- lower server CPU per validated request;
- lower usable cold start and ordinary restart;
- no worse error rate or tail-latency instability.

The primary production comparison is the topology providing the required
isolation. The stronger “Perry wins everywhere” statement additionally requires
Perry to beat the best defensible consolidated Node shape and the equivalent
celld workload, rather than only 100 independent Node processes.

### Deployment gate

- incremental builds have bounded time and memory;
- activation happens before traffic;
- failed deployment leaves the previous version healthy;
- hot replacement drains cleanly and does not leak resources.

### Reliability gate

- the selected production isolation mode has documented crash containment;
- request and deployment resource policies are enforceable;
- restart, rollback, repeated reload, and partial-failure tests pass.

## Test and evidence matrix

| Layer | Required proof |
|---|---|
| Frame/manifest unit tests | Roundtrip, malformed/truncated/trailing input, integer overflow, byte limits, unknown ABI/version, exact exports, source/package/provider identity. |
| Loader integration | Provider order and single-load behavior, missing/modified provider, missing/modified app, wrong target/ABI, manifestless/v1 rejection, eager symbol binding. |
| Deployment integration | Cache hit/miss, failed compile cleanup, immutable publication, health failure, successful swap, old-generation drain, rollback, retention, and crash recovery. |
| Entry lifecycle | HTTP sync/async/error/overload/timeout; cron activation/overlap/replacement; queue ack/nack/DLQ/lease expiry/restart/replacement. Run in-process and worker/shard modes where supported. |
| Reliability soak | Hundreds of replacements plus sustained traffic, process restart during every deployment phase, executor panic, worker crash, database outage, full queue, RSS kill, and controlled disk pressure. |
| Compatibility | #8034 oracle plus focused Perry fixtures under executable/app-dylib modes, normal/forced GC, cold/warm, and concurrent execution. |
| Performance | Isolated Linux 1/10/100 runs with exact artifacts, five trials, validated responses, RSS/PSS/cgroups, CPU/request, percentiles, errors, and raw environment metadata. |
| Security/boundary | Path/symlink traversal, untrusted manifest fields, oversized frames, forbidden exports/dependencies, capability enforcement, and secrets/log redaction. |

CI may use smaller smoke counts, but release evidence must retain the full raw
outputs. Conditional tests that skip when generated providers/apps are absent
do not count as proof unless the release job first builds those artifacts and
asserts their presence.

## Risk register

| Risk | Consequence | Mitigation / closure evidence |
|---|---|---|
| Perry thread-local runtime imposes one thread per app | Consolidated Node wins warm memory or scheduler cost | Attribute TLS/stack/private pages; pursue explicit runtime contexts and bounded executor pools if it is the largest component. |
| Native app failure shares daemon address space | One app can remove the fleet | Trust classes, bounded shards, supervision, cgroups, and an isolated mode for untrusted or hard-deadline workloads. |
| Synchronous native code cannot be preempted in-process | False timeout/resource guarantee | State the limitation, use cooperative deadlines where possible, and require worker/shard mode for hard termination. |
| Latest Perry main moves during development | Irreproducible packages and results | Candidate worktree plus exact promotion gate; provider/app manifests and evidence always carry the commit. |
| Next correctness fixes improve a synthetic fixture only | Invalid full-Next claim | One shared production black-box oracle, focused repros only as supplements, and audit for bypasses/fabricated output. |
| Compilation consumes multi-GiB memory/minutes | Deployments starve serving apps | Separate bounded compiler service/process, wall/RSS budgets, cancellation, incremental cache, and prebuild workflow. |
| macOS fresh-image activation scales poorly | Slow deploys confused with cold request latency | Keep deployment/restart metrics separate; profile signing/layout/dyld and use Linux production evidence. |
| Provider trust-cache shortcut weakens integrity | Modified runtime loads without detection | Signed immutable package/image verification or retain full hashing; fail closed on any identity mismatch. |
| Durable queue lifecycle still lacks long-soak proof | Pool exhaustion, task/lease growth, or shutdown regressions appear only under sustained concurrency | Functional adverse paths pass through real daemons in trusted, dedicated, and sharded modes, including retry/DLQ, killed-delivery recovery, replacement/rollback leadership, reconnect, operator replay/purge, provider enqueue, and exact raw-byte delivery; retain the long pool/PSS/task/lease soak as closure evidence. |
| At-least-once queue replays side effects | Duplicate external actions | Stable delivery IDs, documented idempotency contract, bounded retries, and operator-visible DLQ/replay. |
| Aggregate RSS miscounts shared pages | Misleading Perry/Node verdict | Linux PSS and cgroup memory alongside RSS, with equivalent process topology and isolation. |
| Labels/logging add hot-path cost or cardinality | Performance/memory regression and unusable metrics | Fixed labels, sampled phase tracing off path, benchmark metrics enabled, and cardinality/load tests. |

## Production rollout

1. **Developer preview:** tiny and deterministic handlers only; exact pinned
   packages, manual rollback, no durability claim, explicit in-process trust.
2. **Trusted canary:** small app count on an isolated Linux host with alerts,
   immutable rollback/GC, admission limits, lifecycle soak, and shadow response
   validation against the existing service.
3. **Sharded beta:** bounded failure domains, supervised shard restart, cgroup
   limits, durable queues, database migrations, automated rollback, and mixed
   traffic/deploy failure drills.
4. **Next canary:** only after #8040 is green; mirror the production Next
   request corpus and compare output before accepting traffic.
5. **Production:** success gates pass on the chosen isolation class, capacity is
   derived from PSS/CPU/tail-latency headroom, rollback is rehearsed, and raw
   benchmark/reliability evidence is retained with the exact release identity.

Rollout must include stop conditions for error/tail-latency regression, memory
growth, crash loops, queue age/DLQ growth, activation failure, and response
divergence. Automatic rollback must not delete the failing package until its
diagnostics have been retained.

## Ordered execution plan

| Phase | Work | Exit condition |
|---|---|---|
| 0. Hosting mechanism | Provider split, ABI v2, preload, boundary validation, initial benchmarks | Completed for current macOS and native Linux fixtures |
| 1. Linux truth | Native providers, app compile/load, ELF audit, PSS/cgroup 1/10/100 Perry and Node tiny-app matrix | Diagnostic raw results retained; five-trial production-host run and green workflow remain |
| 2. Runtime density | Attribute and reduce per-app ready/warm memory and thread cost | Perry wins the required-isolation memory comparison |
| 3. Lifecycle safety | Admission/deadlines, replacement soak, immutable rollback/retention/GC, compile budgets | Bounded failure paths and no retained-resource trend |
| 4. Durable background work | Host-owned enqueue identity, Postgres migrations, leasing/retry/DLQ, cron overlap policy | Adverse-path, rollback, outage, and soak suites complete the already passing restart/replacement proof of at-least-once semantics |
| 5. Isolation and operations | Trust classes, process shards, cgroup limits, supervision, metrics/alerts | Production default and guarantees are documented and enforced |
| 6. Next correctness | Land all compiler/runtime compatibility gates using the shared fixture | Full production App Route output matches Node |
| 7. Production proof | Equivalent tiny/compute/Worker/Next matrices under controlled Linux conditions | Every applicable success gate has published evidence |

Phases 1–5 and the Next compatibility work can proceed in parallel. The final
full-Next optimization cannot be interpreted until Next correctness lands, but
tiny-app density, startup, Linux packaging, compilation, lifecycle, durable
queues, isolation, and observability do not depend on it.

### Independent non-Next.js critical path

This is the server/runtime lane to execute while compatibility workers own
#8034–#8040. Each step should land with tests and updated raw evidence before
the next step relies on it.

1. **Establish Linux truth — native diagnostic gate complete.** Native provider
   packaging, ELF/app-boundary audit, immutable restore, lifecycle/100-app
   controls, and the Perry/consolidated-Node/isolated-Node PSS/cgroup matrices
   pass with retained exact identities. Repeat for five trials on the
   production host/version and obtain a green `linux-shared-runtime.yml` run.
2. **Close the measured Perry Buffer leak.** The 250-cycle full-daemon
   sustained-traffic replacement run is implemented and passes atomicity,
   traffic, runtime-reuse, package-retention, compiler, native-image, thread,
   and descriptor assertions. Its positive RSS/live-arena slope is traced to
   TENURED host ABI Buffers. Fix rooting/reclamation or safe full-GC pacing in
   Perry, make `host_buffer_churn_is_reclaimed_by_perry` green, then require a
   flat latter-half arena/PSS slope from both request-only and replacement
   soaks.
3. **Admission and byte limits — core implemented.** Per-deployment concurrency
   now spans all entry types; deterministic overload and HTTP/ABI body/response
   limits are enforced. In-process queue occupancy is sampled; worker backlog
   telemetry and durable-queue deferral remain.
4. **Deadline semantics — core implemented.** All entry types use wall
   deadlines, timed-out native work retains admission, and uncertain worker
   connections are poisoned/replaced. Hard termination remains available only
   at worker/shard boundaries. The `trusted`/`sharded`/`dedicated`/`inherit`
   contract is implemented; complete native Linux hard-timeout and
   sibling-recovery proof for that failure domain.
5. **Prove immutable operations — full local crash-image/SIGKILL proof implemented.** Active/previous
   package state, exact config/static snapshots, audited rollback, exact restart
   restoration, retention pins, startup reconciliation, safe collection,
   status, metrics, real daemon integration, deterministic durable-boundary
   crash images, and compiler/staging/activation/package/state/trash
   child-process SIGKILL recovery exist. Repeat the complete matrix on native
   Linux and run the long replacement/collection soak.
6. **Finish compilation economics — deterministic path implemented.** Queue,
   phase, failure, Perry object and Coop package/compiled-image cache,
   wall/RSS, and bounded-output metrics;
   process-group termination; exact local/dependency snapshots; audited argv
   and environment; stable cross-deployment cache; mutation rejection; and
   explicit prebuild exist. Retain controlled cold/no-op/one-module/static-only/
   validation/activation/rollback evidence and run the cache/lifecycle soak.
7. **Prove durable queues — functional gate implemented.** The host-owned enqueue
   identity, Postgres migrations, transactional leases, retries/backoff, DLQ,
   JSON/raw producer APIs, queue policies, metrics, active consumer generation,
   and authenticated DLQ controls are implemented. Real-daemon in-process and
   worker and shard delivery, restart, replacement, rollback, retry/DLQ,
   killed-delivery recovery, backend termination, full database stop/start,
   fail-closed outage behavior, and pool saturation telemetry pass. Consumer
   retries back off to a five-second cap and sample warnings at powers of two,
   preventing the previous 10-ms outage retry storm. Repeat the stop/start gate
   on native Linux and finish the long concurrency/pool/PSS soak.
8. **Complete operations telemetry.** Core phase, memory/cgroup,
   overload/deadline/queue/artifact/shard signals plus repository-validated
   dashboards and alerts, Prometheus-native synthetic tests, and one-to-one
   operator runbooks are implemented. All label keys are policy checked and
   the optimized direct request metric path passes its cost gate; disposable
   live Prometheus/Grafana provisioning also passes. Configure environment
   receivers and repeat the provisioning/metric-cost gates on production
   Linux.
9. **Optimize the next measured bottleneck.** Host-ABI Buffer retention is the
   current measured blocker. After it is fixed, use Linux private-memory and
   phase profiles to choose among explicit Perry contexts, executor pooling,
   loader parallelism, verification trust caching, allocator tuning, or
   framing reuse. Do not optimize from aggregate RSS alone.
10. **Run the mechanism verdict — common Worker matrix complete.** Perry,
    plain Node, and celld now execute the same checksum/JSON oracle for five
    trials with exact cgroup membership and response validation. Perry wins
    warm PSS and beats Node startup, celld wins startup, and Node wins CPU.
    Repeat on the selected production host/Node version and keep this labeled
    as a mechanism comparison.

### Next.js compatibility lane

1. Commit #8034's Node oracle and expected-fail Perry integration gate.
2. Confirm the selected Perry revision includes #8035 and is clean under the
   integration fixture.
3. Land #8036 request preservation and validate the landed #8039 lazy-module
   implementation against the unmodified #8034 provider-hosted oracle.
4. Complete #8037 async-context isolation and #8038 real response/stream
   return; candidate `main` contains focused async-context work, but the open
   integration issue remains the acceptance authority.
5. Remove every expected-fail, direct-user-handler call, fabricated response,
   eager-route workaround, and compatibility fallback.
6. Pass ten cold starts, two verifier passes per process, the 20-way concurrent
   corpus 100 times, normal GC, and forced/verified GC.
7. Only then build 1/10/100 identical and 100-distinct full Next artifacts for
   the final Perry versus production Node comparison.

### Dependency graph

```text
provider/app boundary ──┬── Linux proof ── runtime-density optimization ─┐
                       ├── immutable deployment ── rollback/GC ──────────┤
                       ├── executor lifecycle ── limits/isolation ───────┤
                       └── cron/queue ABI ── durable queue service ──────┤
                                                                         ├─ production proof
#8034 oracle ── request/lazy/context/response compatibility ── Next gate ┤
                                                                         │
common validated fixtures + common Linux harness ────────────────────────┘
```

## Immediate next actions

1. Obtain the first green `linux-shared-runtime.yml` run; the equivalent native
   commands and exact provider/app identities are already green and retained.
2. Expand the retained two-trial PSS/private-dirty/cgroup-aware 1/10/100 Perry,
   consolidated Node, and isolated Node matrix to five trials on the production
   host and selected Node version.
3. Fix Perry host-ABI Buffer reclamation, make the 50,000-request promotion gate
   green, and rerun the existing 250-cycle full-daemon replacement soak until
   its latter-half arena/PSS slope is flat.
4. Repeat the now-passing dedicated and two-app sharded hard-timeout plus
   immediate-successor-SIGKILL matrix under delegated Linux cgroups; retain
   recovery duration, cgroup events, transport backlog, and route availability.
5. Repeat the locally passing compiler-start, validated-staging,
   post-probe/pre-state activation, package-publication,
   state-temporary/state-rename, and trash-rename SIGKILL matrix on native
   Linux, then run the long immutable-package, rollback, restart, retention,
   and collection soak.
6. Re-run the implemented cold, no-op, one-module, dependency-only,
   static-only, activation, and rollback compiler matrix on isolated Linux;
   retain Perry object-cache and Coop reuse data, CPU, wall, PSS/cgroup peak,
   and phase timings, then run the cross-deployment cache/lifecycle soak.
7. Run the durable-queue concurrency and long-duration soak while sampling
   pool use/saturation, background-task and consumer counts, leases, PSS, and
   shutdown/drain time; repeat the now-passing full PostgreSQL stop/start gate
   on the Linux production runner and retain recovery/resource evidence. The
   functional adverse paths are already proven in trusted, dedicated, and
   sharded execution.
8. Put bounded shards through delegated-cgroup Linux OOM/PID/CPU,
   repeated-crash, hot-replacement, and saturation soaks.
9. After Buffer reclamation is green, validate the system-allocator/128-KiB-
   arena profile with allocation-heavy apps and optimize the next-largest
   measured private-memory component.
10. Repeat the retained five-trial identical-Worker Perry/Node/celld matrix on
    the selected production host and Node version, ideally with randomized or
    interleaved server order to expose host-load/order effects.
11. Merge Next compatibility fixes into #8034's common oracle as they land, then
    run the full production Next matrix only after the tracker is green.

## Reproduction entry points

```sh
# Pin and package Perry providers
scripts/sync-perry-main.sh
scripts/build-perry-libraries.sh

# Linux production package; captures the exact content-addressed directory.
provider_dir="$(sudo scripts/install-perry-provider-package.sh \
  var/coop/lib /opt/coop/providers)"

# Build server binaries and the reproducible tiny Perry app
cargo build --release -p coop-daemon -p coop-worker
scripts/prepare-resource-benchmark.sh
metadata="$(mktemp)"
scripts/capture-linux-benchmark-environment.sh > "$metadata"
mv "$metadata" benchmarks/results/linux-benchmark-environment.txt

# On delegated cgroup-v2 hosts, start the complete command topology inside the
# delegated subtree before asking the harness to create worker/benchmark groups
COOP_DELEGATED_CGROUP_ROOT=/sys/fs/cgroup/coop-delegated \
scripts/run-in-delegated-cgroup.sh cargo test -p coop-host-abi

# Prebuild and verify immutable packages without activation
cargo run --release -p coop-daemon -- \
  --config var/coop/runtime.toml build --all

# Immutable package, rollback, static snapshot, restart, and compiler-budget proof
cargo test -p coop-daemon artifacts -- --nocapture
# Also covers two-app shard residency, group crash recovery, sibling-safe
# unload, and idle-shard cleanup after daemon SIGKILL
cargo test -p coop-daemon --test auto_compile -- --nocapture

# Validate the real shared-runtime loader
cargo test -p coop-worker --test plugin_roundtrip -- --nocapture

# Disposable pinned-Postgres queue gate, including full database stop/start
scripts/run-durable-queue-integration.sh

# Perry promotion gate: intentionally fails on the pinned revision until
# host request/response Buffers are safely reclaimed
cargo test -p coop-worker --test plugin_roundtrip \
  host_buffer_churn_is_reclaimed_by_perry -- --ignored --nocapture

# Compilation matrix plus byte-identical full-daemon replacement soak
node benchmarks/compilation-economics.mjs \
  --trials 1 --soak-cycles 250 \
  --output benchmarks/results/lifecycle-soak.json

# Perry 1/10/100 resource matrix
COOP_BENCH_APP_COUNTS=1,10,100 \
COOP_BENCH_TRIALS=5 \
COOP_BENCH_REQUESTS=20000 \
cargo test --release -p coop-daemon --test resource_benchmark \
  measure_in_process_startup_and_rss -- --ignored --nocapture

# Consolidated and isolated Node matrix
COOP_BENCH_NODE_SCENARIOS=1x1,1x10,1x100,10x1,100x1 \
COOP_BENCH_TRIALS=5 \
COOP_BENCH_REQUESTS=20000 \
cargo test --release -p coop-daemon --test node_resource_benchmark \
  measure_node_process_startup_and_rss -- --ignored --nocapture

# Equivalent Worker-shaped Perry/Node/celld mechanism comparison. Build the
# exact celld-main.lock revision first and provide esbuild 0.28.0. Point Perry
# at the exact directory printed by the provider installer.
export COOP_DELEGATED_CGROUP_ROOT=/sys/fs/cgroup/coop-delegated
export COOP_BENCH_CGROUP_ROOT=$COOP_DELEGATED_CGROUP_ROOT/benchmarks
export COOP_BENCH_PROVIDER_VERIFICATION=root_owned_immutable
export COOP_BENCH_RUNTIME="$provider_dir/libperry_runtime.so"
export COOP_BENCH_STDLIB="$provider_dir/libperry_stdlib.so"
CELLD_ESBUILD=/absolute/path/to/esbuild-0.28.0 \
COOP_BENCH_INCLUDE_CELLD=1 \
COOP_BENCH_TRIALS=5 \
COOP_BENCH_REQUESTS=20000 \
scripts/run-in-delegated-cgroup.sh \
  scripts/run-worker-mechanism-benchmark.sh
```

`BENCHMARKS.md` contains the current measurements and caveats.
`SHARED_RUNTIME.md` documents the implemented library contract and operational
shape. This document is the roadmap and should be updated whenever a gate is
completed, invalidated, or materially re-scoped.

## Parallel ownership boundaries

To minimize conflicts between workers:

- Perry compatibility workers own compiler/runtime fixes and minimal Next
  reproducers in the Perry repository.
- Coop hosting workers own provider loading, app lifecycle, ABI enforcement,
  executor behavior, and hot replacement.
- Benchmark workers own fixtures, harnesses, Linux environment capture, and raw
  result files; they do not rewrite compatibility code to improve a score.
- Operations workers own packaging, cache policy, sharding/isolation, resource
  controls, and CI.

Every result should identify which lane changed and rerun the smallest relevant
correctness test before the shared performance matrix.
