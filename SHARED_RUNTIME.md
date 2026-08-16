# Perry application libraries in Coop

The complete implementation roadmap and benchmark gates are in
[`PERRY_SERVER_PLAN.md`](PERRY_SERVER_PLAN.md).

Coop runs Perry-compiled applications as preloaded shared libraries. Runtime
code is packaged once per Coop process rather than copied into each app:

```text
coop
├── libperry_runtime.{dylib,so}  one runtime implementation in the process
├── libperry_stdlib.{dylib,so}   one fs/http/db/crypto/etc. implementation
└── compiled/
    ├── app-001.{dylib,so}       application code only
    ├── app-001.coop-lib.json   exact ABI + entry-point descriptor
    └── ...
```

Each application gets a dedicated OS thread because Perry runtime state is
thread-local. The daemon loads, initializes, and warms every app on that thread
before publishing it to the router. A request performs only a router lookup, a
channel handoff, and a native function call—there is no process spawn, `dlopen`,
module initialization, or compiler work on the request path.

“Zero latency” here means zero cold-start latency. Execution and the thread
handoff still take real CPU time. The end-to-end smoke run measured
0.384 ms mean HTTP time across ten concurrent requests on the development
machine (1,000 requests, no failures).

## Build from the pinned Perry main

Coop pins the exact main revision in `perry-main.lock` and keeps an ignored,
detached worktree at `.perry-main`. To refresh and package it:

```sh
scripts/sync-perry-main.sh
scripts/build-perry-libraries.sh
cargo build -p coop-daemon -p coop-worker
```

The packaging script supports native macOS and Linux builds. It produces these
files in `var/coop/lib/`:

- `libperry_runtime.dylib` and `libperry_stdlib.dylib` on macOS;
- `libperry_runtime.so` and `libperry_stdlib.so` on Linux;
- `perry-libraries.json`

The version-2 provider manifest records the exact size and SHA-256 digest of
both provider files in addition to the Perry/compiler/toolchain identity. The
host verifies those bytes before either file is loaded and fails closed on a
mismatch.

Portable `full_hash` mode checks both independent files concurrently on the
first initialization in a host process. Later initialization calls canonicalize
the requested paths and return immediately when they match the already resident
pair and verification policy; they do not hash the same 95 MB again.

Linux production images may install a fully hashed package into a root-owned
content namespace:

```sh
provider_dir="$(sudo scripts/install-perry-provider-package.sh \
  var/coop/lib /opt/coop/providers)"
```

The installer validates the source sizes and SHA-256 digests, stages and
revalidates root-owned files, and atomically publishes
`/opt/coop/providers/<manifest-sha256>`. `root_owned_immutable` then replaces
the repeated large-file read with a fail-closed OS trust check: Coop must run
unprivileged, and the canonical package directory, every ancestor, manifest,
runtime, and stdlib must be root-owned and not writable by the service, group,
others, or an ACL. Writable development paths and root-running Coop are
rejected. `full_hash` remains the default.

Provider packaging defaults Perry's arena block size to 128 KiB and records
that build profile in the manifest. Set `PERRY_ARENA_BLOCK_SIZE_BYTES` to a
power of two from 128 KiB through 1 MiB to build a different profile. The
packaging script restores the pinned Perry source after the provider build.
The provider allocator is also explicit: dense server packages default to the
system allocator; set `PERRY_PROVIDER_ALLOCATOR=mimalloc` to trade additional
per-thread resident memory for Perry's faster allocation path. The selected
allocator is part of the provider manifest.

The manifest pins the Perry version, exact git commit, compiler SHA-256, Rust
toolchain, target, and filenames. Coop loads runtime first and stdlib second
with global symbol visibility, verifies that stdlib is bound to that exact
runtime image, then allows application libraries to load. The daemon also
hashes the configured compiler once and refuses to compile if it is not the
compiler packaged with the provider pair.

Perry's stdlib currently calls both the public C ABI and Rust-level runtime
APIs. The packaging linker therefore retains generic Rust glue where needed,
but marks the runtime surface interposable so every stateful call resolves to
the process-first `libperry_runtime` image. Both files must always be built and
deployed as one pair.

## Runtime configuration

In-process hosting is the default. The relevant `runtime.toml` settings are:

```toml
[execution]
mode = "in_process"
provider_verification = "full_hash" # or "root_owned_immutable" on installed Linux packages
watch_deployments = true # set false for API-only orchestration
preload_concurrency = 1 # macOS; the default is 4 on other platforms
compile_concurrency = 1
compile_dependency_max_files = 250000
compile_dependency_max_bytes = 4294967296
compile_march = "generic"
executor_stack_size_bytes = 1048576
command_queue_capacity = 256

[execution.cgroup]
mode = "auto" # "required" fails worker/shard activation closed
root = "/sys/fs/cgroup/coop"

[execution.shards]
count = 4
max_apps = 25
max_rss_mb = 1024
max_cpu_percent = 400
max_pids = 256

[paths]
perry_binary = ".perry-main/target/perry-dev/perry"
perry_runtime_library = "var/coop/lib/libperry_runtime.dylib"
perry_stdlib_library = "var/coop/lib/libperry_stdlib.dylib"
```

The example uses macOS filenames; Linux uses the corresponding `.so` files.
For `root_owned_immutable`, both library paths must point inside the exact
directory printed by the installer. Dedicated and sharded workers receive the
same policy from the daemon.

Each deployment can bound admission and ABI allocations in `coop.toml`:

```toml
[isolation]
class = "trusted" # or "sharded", "dedicated", "inherit"

[limits]
max_wall_clock_ms = 30000
max_concurrent_invocations = 256
max_request_body_bytes = 8388608
max_request_header_bytes = 65536
max_response_body_bytes = 8388608
max_response_header_bytes = 65536
max_queue_payload_bytes = 1048576
max_worker_rss_mb = 512
max_worker_cpu_percent = 100
max_worker_pids = 64

[activation]
path = "/health?deep=1"
method = "GET"
requests = 2
expected_status = 200
# expected_body_sha256 = "..." # optional exact semantic check
```

Isolation is a deployment property, not an accidental consequence of which
daemon it lands on:

- `class = "trusted"` runs the application on its thread-affine executor in
  the daemon address space. It provides the density and direct-call path but
  cannot contain a native crash or hard-kill synchronous native work.
- `class = "dedicated"` gives the deployment one supervised `coop-worker`
  process. A crash or hard deadline can remove that process without removing
  the daemon or other dedicated deployments.
- `class = "sharded"` gives the deployment an independently addressed runtime
  inside one bounded, supervised multi-app worker. The deployment has a stable
  SHA-256 preferred slot and deterministically probes other slots at capacity.
  A native crash or hard termination recreates every app in that shard, while
  leaving the daemon and other shards alive.
- Omitting the block, or `class = "inherit"`, resolves through the box-wide
  `[execution].mode` setting for backward-compatible fleet defaults.

The resolved class is immutable package configuration. Changing it performs a
real warm replacement even when application bytes are identical; an
in-process runtime can never be reused for a package requesting a worker or
shard, nor can a generation cross any other execution-mode boundary. The
authenticated deployment memory endpoint reports `requested_isolation_class`,
`effective_isolation_class`, `execution_mode`, worker PID, and shard
slot/generation where applicable. Prometheus exposes numeric process-isolated,
sharded, and inherited-policy gauges without mutable class labels.

HTTP, cron, and queue work share the per-deployment admission semaphore. The
effective concurrency is capped by the executor command-channel capacity, so a
configuration cannot advertise more queued work than the host can retain.
Overloaded HTTP calls receive `503`, `Retry-After: 1`, and
`x-coop-error: overloaded`. Request body/header violations receive `413`/`431`,
oversized application responses receive `502`, and wall deadlines receive
`504`. Every complete application frame remains subject to the 16 MiB ABI
ceiling even when a component limit is larger.

An in-process native call cannot be preempted safely. If it exceeds its wall
deadline, the caller receives `504` but the call retains its admission permit
until the executor really completes it. In worker or shard mode, a timed-out or
malformed transport is permanently poisoned and its uniquely named failure-
domain generation is replaced; the connection is never reused for a later
response. A shard timeout necessarily interrupts sibling applications, so
hard-deadline workloads requiring one-app containment should use `dedicated`.

Shard load control is idempotent only when both the runtime ID and complete
deployment specification match. A lost response is retried with that exact
identity under a bounded control timeout. Reusing an ID for different bytes,
limits, module identity, context, or queue policy fails closed. If retries still
cannot distinguish “not loaded” from “loaded but response lost,” Coop poisons
and terminates the complete shard failure domain rather than releasing capacity
while leaving an unreachable executor resident. A definitive application
rejection does not disturb healthy siblings.

When `[activation].path` is present, Coop dispatches one to 64 sequential
requests directly against the initialized replacement before writing active
artifact state or changing routing. The ordinary deployment deadline and byte
limits apply. Every response must match `expected_status` and the optional body
digest. Failure drains the unpublished generation and leaves the current one
untouched. `GET /_coop/admin/deployments/<name>/health` exposes the last live
generation's outcome, count, status, duration, and completion time; activation
counters/histograms expose success, failure, and exact no-op reloads. Restart
and rollback rerun the packaged generation's exact probe. An exact package
reload of an already healthy generation starts no executor and reruns no probe.

`mode = "worker"` retains process isolation and the Unix-socket transport. The
daemon passes the same provider pair to every worker. Initial app loads are
bounded by `preload_concurrency`; memory-intensive Perry builds have their own
lower `compile_concurrency` bound. The macOS default is one because dyld
serializes image loading internally; other platforms default to four.

For new mixed-trust installations, prefer the per-deployment `[isolation]`
block and treat `[execution].mode` as the default for deployments that inherit.
`mode = "shard"` makes bounded sharding the inherited default. Shard processes
start lazily, load the provider pair once, and accept generation-scoped
load/unload plus HTTP/cron/queue dispatch. The distinct-app limit still permits
old and new generations of one deployment to overlap during atomic activation.
If a preferred shard is full, deterministic probing uses remaining fleet
capacity; once every configured slot is full, activation fails before
publication.

On Linux, dedicated and sharded generations use cgroup v2 when the configured
hierarchy is delegated. Coop creates a unique cgroup before spawn and writes
`memory.max`, `memory.swap.max = 0`, `memory.oom.group = 1`, `cpu.max`, and
`pids.max`. The worker moves itself through that generation's `cgroup.procs`
before it loads either Perry provider or application code. Dedicated limits
come from the deployment; shard limits are aggregate values from
`[execution.shards]` and are not per-app promises. `mode = "auto"` records a
visible fallback and retains the process-RSS watchdog if delegation is
unavailable; `mode = "required"` rejects activation instead of promising
unenforced limits; `disabled` opts out explicitly. The admin memory response
and Prometheus metrics expose cgroup current/peak memory, CPU use, PID count,
OOM/OOM-kill events, and shard residency. Idle worker/shard exit is detected
without waiting for a request and replacement attempts use bounded exponential
backoff.

Every daemon-spawned dedicated worker and shard is also bound to the exact
daemon PID. Linux uses a kernel parent-death signal and all Unix builds verify
parent identity on a short interval, so even an idle worker exits after daemon
SIGKILL instead of becoming an orphan. Manual worker invocations may omit this
binding explicitly.

## Compatibility and hot reload

Every compile writes `<app>.coop-lib.json` with:

- Coop app ABI version;
- exact Perry compiler/runtime version, commit, and compiler SHA-256;
- exact dereferenced dependency-tree and semantic compiler-invocation digests;
- target architecture and OS;
- module initializer symbol;
- exact required Buffer handler symbol and calling convention;
- application byte length, SHA-256, and deployment-time boundary-verification
  status.

Coop rejects incompatible or incomplete libraries. During a reload, it fully
warms the replacement before swapping it into the router, then drains the old
thread. Existing requests finish without seeing loader latency.

All hosting modes reject manifestless images and every ABI version before v2.
App images may declare dynamic dependencies on the two provider files so the
platform loader can use a two-level namespace, but they may not define any
provider-owned symbol or embed provider code.

The compiler's final link exports only `perry_module_init` and stable
Coop-owned aliases: `coop_app_http_v2`, `coop_app_cron_<index>_v2`, and
`coop_app_queue_<index>_v2` as required by the manifest. Perry's generated,
source-path-derived wrapper names are link-time implementation details and are
never the public app ABI. The link also enables dead stripping, removes
local/debug symbol tables from deployable images, and binds undefined Perry
imports directly to the separately packaged providers. Provider packaging
applies the same symbol-table stripping while retaining every dynamic export
needed across the runtime/stdlib boundary. Coop runs the
dependency/export boundary audit once at deployment time, then uses the
recorded size and SHA-256 to prove that later loads see those exact bytes. This
removes `otool`/`nm` and thousands of symbol comparisons from startup without
trusting a stale sidecar.

App compilation always uses `--no-codegen --no-auto-optimize --march <pinned>`.
Perry already omits runtime/stdlib archives from a dylib link; the second flag
also prevents per-app specialized providers that would be discarded. Codegen
hooks must run upstream and their committed outputs become snapshot inputs.
Coop clears the ambient build environment, passes an audited allowlist, and
hashes its semantic argv, compiler/linker-wrapper/tool bytes, provider bytes,
target, CPU baseline, and propagated environment into the package identity.

Every HTTP app must export `handle(request: Buffer): Buffer` (or the async
`Promise<Buffer>` form). The versioned `COOP` frame carries
method, URL fields, duplicate headers, and raw body bytes; the response carries
status, duplicate headers, and raw body bytes. There is no JSON/Base64 app ABI
or legacy fallback. ABI v2 also defines strict cron and queue frames. Cron has
an empty success response; queue requests retain raw payload bytes and responses
return an explicit ack, nack, or dead-letter disposition. The application
manifest names every exported HTTP, cron, and queue entry exactly. Production
scheduler policy is explicit in each `[[crons]]` block:

```toml
[[crons]]
file = "handlers/cron.ts"
schedule = "*/5 * * * *"
timezone = "UTC"
overlap = "skip"       # or "allow", still bounded by deployment admission
late = "skip"          # or "run_once"
max_lateness_ms = 30000
```

The daemon now owns cron lifecycle in every execution mode. It validates every
schedule before a replacement can publish, creates tasks behind an activation
gate, opens that gate only after the new runtime enters the live map, and aborts
all old schedulers and joins dispatched fires before executor/worker drain.
Dedicated and sharded modes use the versioned worker cron message; sharded
dispatch also carries the exact runtime identity. Trusted in-process mode calls
the raw app ABI directly. Cron fires use UTC, never replay daemon downtime, and
default to skipping both
overlap and scheduler wakeups beyond the lateness budget. `run_once` dispatches
one delayed fire; `allow` permits schedule overlap but cannot exceed
the shared deployment admission limit. Generation shutdown joins dispatched
fires through their ordinary wall deadline. Dispatch, skip reason, lateness,
completion, failure, overload, and timeout metrics are emitted per configured
schedule. Queue delivery is likewise implemented in every mode and adds Base64
only at the worker socket boundary. `queue.send()` synchronously commits JSON
bytes through the host-owned provider callback, while `queue.sendRaw()` accepts a Buffer and
crosses the native application/provider boundary as pointer plus length with no
JSON or Base64 transform. The configured Postgres service owns claiming,
visibility leases, bounded retry/backoff, dead-letter storage, and generation
lifecycle. Queue deployments fail closed when that service is unavailable
unless the narrow ABI-only tooling escape hatch is explicitly enabled.

Dead-letter inspection is authenticated, paginated, and metadata-only so it
does not turn the admin API into a payload or secret exfiltration path. Replay
and purge additionally require explicit confirmation headers, preserve audit
logs without payload contents, and increment bounded-cardinality operator
metrics. The real-daemon suite proves exact raw-byte retention, delivery in
trusted and dedicated execution, restart, replacement, rollback, retry
exhaustion, explicit dead-lettering, killed-delivery lease recovery,
connection-pool recovery, replay, and purge. The same suite contains a sharded
producer/consumer branch that is compile-checked; retaining a Postgres-backed
run remains an explicit evidence gate.

## Verifying the split

On macOS, a compiled app should list the two `@rpath/libperry_*` providers and
`libSystem` in `otool -L`, retain undefined Perry imports in `nm -u`, and expose
only its two manifest-declared entries in `nm -gU`. On Linux, use `readelf -d`
for `NEEDED` entries and `nm -D` for dynamic imports and exports. The host
executable should contain no Perry imports or exports. All app imports must be
present in the union of runtime and stdlib exports. The daemon integration
tests exercise compilation, manifest generation, integrity validation, preload,
strict binary dispatch, and HTTP routing against this layout.

Automatically compiled applications are immutable packages. Coop computes a
source identity from sorted relative source/config/lock paths, exact bytes, the
serialized deployment configuration, and the complete installed dependency
tree. Dependency symlinks are dereferenced into the private snapshot, so linked
workspace package bytes cannot change underneath the compiler. Perry's own
machine-local cache is the sole excluded subtree; file/byte limits bound both
hashing and copying. Compilation, export validation, boundary audit, library
hashing, and manifest creation all occur under
`compiled/.staging/`. After syncing the library, manifest, and directory, Coop
publishes both files together with one same-filesystem rename to:

```text
compiled/<deployment>/<package-sha256>/app.{dylib,so}
compiled/<deployment>/<package-sha256>/app.coop-lib.json
```

The package digest covers the complete ABI manifest, which contains the source
identity and exact library digest. Failed builds remove their staging directory
and cannot mutate the currently loaded path. A matching package is a
timestamp-independent cache hit. Source-less deployments can restore an
already published immutable package, but there is no legacy mutable-library
fallback and the compiler never writes a mutable deployment path.

When only configuration or static bytes change and the packaged application
image has the same verified SHA-256, Coop probes and reuses the already
initialized healthy runtime instead of loading a second native image. It still
records the new immutable package, swaps routing/admission/configuration
atomically, stops the old cron/queue generation, and activates the new
background generation. Reuse is forbidden when the effective isolation mode,
queue ABI policy, or payload ceiling changes, or when the current worker
transport is poisoned.

The implemented 250-cycle daemon replacement soak proves 250/250 such runtime
reuses, 21,740 concurrent validated requests with no errors, no compiler or
second app load, bounded packages, and stable thread/descriptor counts. It is
not yet a memory pass: the pinned Perry runtime retains one TENURED host
request Buffer and one response Buffer per invocation (152 live arena
bytes/request in a 50,000-request probe). Minor GC cannot reclaim these old-
generation objects, and forcing a full GC from Coop corrupted the next ABI
response, so Coop does not ship that workaround.
`host_buffer_churn_is_reclaimed_by_perry` is the ignored promotion gate that a
new Perry candidate must make green.

Perry's content-keyed object cache lives under
`compiled/.perry-cache/<compiler-contract>/`, shared safely by deployments with
the same exact compiler contract. Coop records Perry object hit/miss counts,
Coop package and compiled-image reuse, and identity, snapshot, compiler,
validation, package, and publication phase durations. Perry's dylib report does
not currently expose a distinct link-cache result.

The opt-in capacity smoke creates 100 distinct app-library images and executor
threads, then dispatches to all of them concurrently:

```sh
scripts/prepare-resource-benchmark.sh
cargo test -p coop-worker --test plugin_roundtrip \
  hundred_preloaded_apps_dispatch -- --ignored --nocapture
```

On the original ABI-v2 process control, first activation of 100 newly created
Mach-O images took 28.89 seconds, while clean daemon restarts over those same
100 eagerly bound and initialized artifacts took 724 and 620 ms. Invoking every
app once took about 89-109 ms and resulted in roughly 131 MiB RSS. The large
first-activation number is deployment work rather than deferred request work;
Coop does not use `RTLD_LAZY`. This validates the hosting shape with a small
Perry app; the size and compatibility of a real Next.js build remain
Perry/compiler concerns rather than a promise of this loader.

The direct 100-app capacity probe now also attributes Perry arenas. With the
system allocator and 128 KiB arena blocks, two runs measured 39.8–40.3 MiB RSS
ready and 101.9–102.5 MiB after invoking every app. The arenas reserved 128 KiB per ready app
and 256 KiB per warm app, while reported live arena data was only 160 and 312
bytes per app. This shows that the system allocator materially improved the
old mimalloc profile, but also that most remaining warm RSS is outside live
arena objects and needs further attribution on Linux.

Process startup, RSS, and fixed-workload comparisons with both consolidated
and process-per-app Node.js baselines are recorded in [`BENCHMARKS.md`](BENCHMARKS.md).
