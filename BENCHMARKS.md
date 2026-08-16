# Coop server-efficiency benchmarks

The complete implementation roadmap and success gates are in
[`PERRY_SERVER_PLAN.md`](PERRY_SERVER_PLAN.md).

These experiments ask whether one Coop process loading Perry application
libraries can be a more memory-, compute-, and startup-efficient server shape
than Node.js, and where celld fits in that comparison.

## Result in one paragraph

The shared-library mechanism works natively on Linux, but a Perry-hosted Next
application is not yet proved competitive with Node. In the corrected
five-trial equivalent-Worker engineering run, Perry reached a validated
response in 55.6 ms versus 147.8 ms for plain Node and 23.0 ms for celld. Perry
had the lowest warm PSS at 28.24 MiB, versus 30.14 MiB for Node and 31.45 MiB
for celld. Node still won server CPU/request at 87.5 us; Perry and celld were
effectively tied in this run at 118.5 and 117.0 us. The result uses an explicit
root-owned immutable provider package; the portable full-hash Perry default
took 565.7 ms usable cold, and parallel full hashing reduced a subsequent run
to 423.4 ms. All three rows execute and validate the same URL, 100-iteration
checksum, and JSON response.

The separate two-trial density matrix still matters: Perry used less warm PSS
than consolidated Node at one and ten tiny apps, but more at one hundred apps
because of per-app executor/runtime state. Perry remained vastly denser than
one Node process per app. Both sets are engineering evidence from a shared
Ubuntu VM, not production-host proof, and celld remains a Worker mechanism
comparison rather than a Next.js host.

## First optimization pass

The first implementation pass focused on costs that do not require changing
Perry's generated application semantics:

- The listener, supervisor, and app executor now retain raw body bytes. Apps
  use a compact versioned `COOP` Buffer frame instead of JSON and Base64.
- Perry's final application link is wrapped with a platform export allowlist.
  The Next fixture dropped from 2,690 public symbols and 4.5 MiB to three public
  symbols and 4.2 MiB.
- `otool`/`nm` boundary validation runs once after compilation. Its result is
  stored with the library size and SHA-256, so startup can prove it is loading
  the same bytes without repeating the symbol walk. Tampered artifacts fail
  before `dlopen`.
- Provider, library-load, module-init, and sampled request phases now emit
  durations; RSS is sampled only off the request path.
- Initial preload is bounded-parallel (default four), while Perry compilation
  has its own conservative bound (default one) because real Next compilation
  can consume several GiB.

The final three-trial rerun used the same 2,000-request workload at concurrency
20. Unfortunately its one-minute load average was 79-81, versus 39-43 in the
baseline, so throughput and wall time are not clean A/B evidence.

| Perry lower bound | Listener start | Usable cold | Warm RSS | Post-2k RSS | Server CPU/request | Requests/s |
|---|---:|---:|---:|---:|---:|---:|
| Before | 1,656 ms | 1,659 ms | 39.6 MiB | 92.5 MiB | 385 us | 1,118 |
| Optimized | 357 ms | 366 ms | 40.0 MiB | 95.0 MiB | 300 us | 1,492 |

The startup phase log is more diagnostic than the cross-load wall comparison:
the cached provider pair took about 69 ms, SHA verification of the 4.2 MiB app
took 16 ms, app `dlopen` 18 ms, and app initialization 50 ms in one sample.
The compact transport's encoding and decoding averaged only a few microseconds;
the Next/Perry invocation still dominates request CPU.

The original 100-tiny-app startup number conflated daemon restart with macOS's
one-time activation of newly linked Mach-O images. The harness recreated and
changed the install name of all 100 images before every trial, forcing that
cost repeatedly. With the exact same 100 artifacts retained across process
trials, their first activation took 28.89 seconds but subsequent fully eager
`RTLD_NOW` daemon starts took 724 and 620 milliseconds. Module initialization
was only about 0.1 ms per tiny app. The long number is real deployment work on
this macOS machine, but it is not normal server restart or request latency.

The harness now stages one artifact set per scenario, labels trial one
`fresh`, and reports restart trials separately. It can also forward all daemon
phase logs with `COOP_BENCH_TRACE_STARTUP=1`. Coop still uses `RTLD_NOW` and
completes module initialization before publishing the listener, so no binding
or initialization cost was moved to the first request.

## ABI-v2 and second optimization pass

ABI v2 removes the compatibility path completely. `handle(Buffer): Buffer` is
the only application HTTP entry point; manifestless and ABI-v1 images fail to
load in the trusted and dedicated execution modes measured in that pass. The
compiler now rejects explicitly string-typed handlers, exports only
`perry_module_init` and `handle`, enables dead stripping, and binds imports to
the separate provider files using macOS's two-level namespace. No runtime or
stdlib code is copied into an application.

The tiny app fell from 68,616 to 51,720 bytes. The Next lower-bound image fell
from 4,437,648 to about 4,158,520 bytes and from three public exports to two. On a
20-image control, serial eager preload took 6.84 seconds versus 7.33 seconds at
concurrency four, so macOS now defaults to serial preload. Other platforms keep
the concurrency-four default.

The strict 100-app control used 59.0 MiB ready RSS and 130.8 MiB after invoking
every app once. The comparable old control used 120.2 MiB ready and 375.5 MiB
warm. This is the clearest gain in this pass: removing each app's JSON/Base64
work avoids substantial per-thread Perry heap growth. The corrected startup
measurement is 28.89 seconds for first activation of freshly created images
and 0.72/0.62 seconds for clean daemon restarts over the same fully eager app
set. These runs were still under substantial system load, so repeat them on an
isolated Linux host before treating wall time as publication quality.

Deployable binaries now discard non-exported local/debug symbols while
preserving their complete dynamic ABI. On the pinned provider pair this reduced
the runtime from 57.3 MB to 53.9 MB and stdlib from 53.8 MB to 43.9 MB; the
4.44 MB Next lower-bound app becomes about 4.16 MB when rebuilt by the updated
link wrapper. The exact stripped app bytes are boundary-checked and hashed in
the manifest.

## Next route comparison

### Workload and environment

Measurements were taken on 2026-08-13 on an Apple M1 Max with 10 physical CPU
cores and 64 GB RAM, using macOS 26.5, Node.js 26.5.1, Next.js 16.3.0, Perry
0.5.1503 at commit `564c56308d221a51b50308d9165578fbb176e877`,
and celld at commit `3f22aedd1ea4d413b93e84afb1ce385f04be84f1`.

The API route performs 100 FNV-style integer iterations and returns a fixed,
validated JSON response. Each result is the median of three fresh process
launches followed by 2,000 keep-alive HTTP requests at concurrency 20.

- Listener startup is spawn-to-TCP-listen.
- Usable cold start is listener startup plus the first validated request.
- Ready RSS is sampled after listen; warm RSS follows the first request.
- Post RSS follows the 2,000-request workload.
- Server CPU is the process-tree CPU-time delta, independent of client CPU.
- RSS is the sum reported by macOS `ps` for the server process tree.

The machine was under heavy unrelated compilation load: median one-minute load
averages were 39-43 during these three side-by-side runs. Wall-clock latency,
throughput, and startup figures are therefore directional, not publication
quality. The per-process RSS differences and server CPU-time deltas are more
useful, but should still be repeated on an isolated Linux server using cgroup
memory and proportional set size.

### Results

| Shape | Listener start | Usable cold | Ready RSS | Warm RSS | Post-2k RSS | Server CPU/request | Requests/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| Coop + Perry Next API lower bound | 1,656 ms | 1,659 ms | 36.5 MiB | 39.6 MiB | 92.5 MiB | 385 us | 1,118 |
| Node production Next standalone | 647 ms | 849 ms | 98.6 MiB | 111.9 MiB | 247.3 MiB | 840 us | 452 |
| celld equivalent Worker | 16 ms | 1,770 ms | 22.4 MiB | 42.2 MiB | 100.5 MiB | 140 us | 8,524 |

The Perry lower bound used about 65% less warm RSS and 54% less sampled server
CPU than Node, but did less framework work. Node ran the real production Next
HTTP and App Route stack. Perry constructed a `NextRequest`, invoked the same
source route, ran `NextResponse.json`, and then emitted the known deterministic
payload directly through Coop's synchronous ABI. Perry did not run Next's
private `AppRouteRouteModule.handle`, AsyncLocalStorage work stores, production
webpack route loader, query parsing, or response stream extraction. These
omissions favor Perry, so the row cannot support a full-Next performance claim.

The initial run's most important negative result was startup. Coop loads the
separate runtime and stdlib providers and completes application module
initialization before it publishes the listener. This is operationally honest,
but the baseline was about twice Node's usable cold start even for the narrowed
route. The optimized rerun brought this below the recorded Node result, subject
to the cross-load caveat above. There is no process spawn on a warm library
call, yet that is not the same as zero deployment latency.

celld publishes its listener almost immediately and lazily activates the Worker
cell. That makes listener startup look excellent while usable cold start is
dominated by the first request. Its workload is an equivalent Worker, not Next,
and the local test used MinIO only as a development S3-compatible store. celld's
documentation requires S3 or GCS for the real fleet and explicitly does not
provide a local filesystem store.

### Perry library boundary

The initial narrowed application artifact was 4.5 MiB; export restriction and
stripping make the rebuilt artifact about 4.16 MiB. The stripped process-wide
runtime and stdlib are separate 53.9 MB and 43.9 MB files. `otool -L` on the app
library lists both provider install names, but their code remains in separate
files and is loaded only once process-wide. Coop loads the exact provider pair
globally, validates the pinned Perry version, commit, compiler hash, Rust
toolchain, and target, then preloads the application library on its dedicated
Perry thread.

The provider split also matters semantically. The runtime is built with Perry's
`stdlib` feature so it does not export fallback `Request`, `Headers`, and
`Response` implementations ahead of the real stdlib provider.

### Compilation cost

Compilation is outside request startup, but it matters for server deploys.

- The narrowed 58-module Next API graph initially built in 25.5 seconds with
  681 MiB max RSS; warm one-module changes linked in 2.3-3.3 seconds with about
  238 MiB max RSS.
- A production-bundle attempt that compiled 120 modules succeeded in roughly
  4-5 minutes and required several GiB of memory in earlier runs.
- Under the day's severe contention, two incremental production-bundle retries
  were bounded after 16.4 and 20.4 minutes. They had reached 2.8 GiB and 8.35
  GiB max RSS respectively without linking.

This cost is paid at build/deploy time rather than per request, but it is not
currently friendly to on-demand application compilation.

The resident-daemon compilation harness at
`benchmarks/compilation-economics.mjs` now makes the deploy-time costs
reproducible. It creates a small Perry application with a real local package,
keeps one Coop daemon resident, disables filesystem watching so every sample
has exactly one explicit trigger, validates the response after each activation,
and measures cold activation, warm no-op reload, one-module change, dependency
change, static-only change, and rollback. It records process-tree CPU/RSS (and
PSS on Linux), compiler peak RSS, build phase timings, object-cache results,
package hits, compiled-image reuse, and whether Perry was spawned.

One complete directional run on the loaded macOS development machine is
retained in `benchmarks/results/2026-08-13-compilation-economics.json`:

| Scenario | Ready wall | Server-tree CPU | Peak tree RSS | Perry spawned | Reuse evidence |
|---|---:|---:|---:|---:|---|
| Cold start + activation | 6,509 ms | 1,560 ms | 301.6 MiB | yes | 0/3 Perry objects |
| Warm no-op reload | 171 ms | 10 ms | 39.3 MiB | no | exact live package |
| One-module change | 5,700 ms | 550 ms | 309.3 MiB | yes | 1/3 Perry objects |
| Static-only change | 395 ms | 30 ms | 41.8 MiB | no | hard-linked app image |
| Rollback | 202 ms | 20 ms | 41.1 MiB | no | immutable package |

The one-module and dependency compilers peaked at 236.1 and 264.8 MiB RSS.
Static-only publication spent about 175 ms in measured build phases and no
compiler CPU. Reusing the verified image as a same-filesystem hard link reduced
its loaded-host wall time from 2,483 ms in the prior loaded-machine sample to
395 ms; app `dlopen` itself fell from seconds to roughly 0.1 ms in the real
integration. An exact healthy reload now skips executor creation and its
identity phase was 1.4 ms; the table's 171 ms includes two process-tree `ps`
samples under contention. The no-op and rollback CPU totals remain tiny
relative to wall time. The machine's one-minute load average was 44.9, and an attempted
three-trial run had to be discarded after a third cold `dlopen` exceeded the
60-second safety timeout. These figures validate fast-path behavior and expose
where wall time remains, but they are not comparison-quality latency numbers.
The isolated Linux matrix must provide the retained median and PSS result.

Reproduce the controlled matrix after building Coop and the pinned provider
pair:

```sh
node benchmarks/compilation-economics.mjs \
  --profile release --trials 5 --timeout-ms 300000 \
  --output benchmarks/results/linux-compilation-economics.json
```

## Tiny 100-app control

The earlier tiny-handler control remains useful because it isolates hosting
shape from Next compatibility. It used the same machine and median-of-three
method, with 20,000 requests at concurrency 50.

| Shape | Startup | Ready RSS | Warm RSS | Post-20k RSS | Requests/s | Server CPU/request |
|---|---:|---:|---:|---:|---:|---:|
| Coop, providers only | 274 ms | 14.3 MiB | 14.3 MiB | - | - | - |
| Coop, 1 app | 574 ms | 17.4 MiB | 22.5 MiB | 34.3 MiB | 56,042 | 27 us |
| Coop, 100 app dylibs | 30.3 s | 120.2 MiB | 375.5 MiB | 387.9 MiB | 29,034 | 38 us |
| Coop ABI v2, 100 fresh app images (first activation) | 28.89 s | 59.0 MiB | 130.8 MiB | - | - | - |
| Coop ABI v2, restart over same 100 eager images | 0.72 s | 59.0 MiB | 130.8 MiB | - | - | - |
| Node, 1 process / 1 app | 63.6 ms | 64.7 MiB | 66.1 MiB | 83.8 MiB | 45,305 | 28 us |
| Node, 1 process / 100 logical apps | 62.5 ms | 64.5 MiB | 67.9 MiB | 80.0 MiB | 48,230 | 27 us |
| Node, 100 processes / 100 apps | 1.01 s | 6,481.9 MiB | 6,619.9 MiB | 6,895.5 MiB | 16,753 | 277.5 us |

Coop clearly beats 100 independent Node processes on aggregate RSS, but not a
single Node process multiplexing 100 tiny handlers. ABI v2 sharply reduced RSS.
The corrected harness shows that ordinary restart of 100 eagerly bound images
is sub-second on this machine; creating and activating 100 brand-new Mach-O
images still incurs a large one-time deployment cost. The real product question
is therefore whether separate Next deployments can share enough provider state
while retaining useful isolation, not whether native code beats a deliberately
fragmented Node topology.

The retained-image executor lifecycle control now defaults to 250 measured
load/dispatch/shutdown cycles after five warmups. The 2026-08-13 macOS run
completed in 1.44 seconds: threads stayed 3→3 (peak 3), open descriptors stayed
10→10 (peak 10), and RSS moved from 17,024 to 19,840 KiB. Run a different count
with `COOP_LIFECYCLE_CYCLES`; the ignored test enforces thread, descriptor, and
retained-RSS ceilings:

```sh
cargo test -p coop-worker --test plugin_roundtrip \
  repeated_load_dispatch_shutdown_reclaims_executor_threads \
  -- --ignored --nocapture
```

## Reproduce the Next fixture

Install the Next dependencies and build the production Node app once:

```sh
cd benchmarks/next-small
npm ci
npm run build
```

Build and validate the shared provider pair with
`scripts/build-perry-libraries.sh`, then let Coop build the application
library itself:

```sh
cargo build --release -p coop-daemon
scripts/prepare-next-benchmark.sh
```

That script stages `benchmarks/next-small/coop/coop.toml`,
`coop/coop-handler.ts` (as `handlers/main.ts`), and
`app/api/benchmark/route.ts` into `target/next-benchmark/coop-run`, links the
installed dependency tree, and runs the daemon until it has compiled,
published, and loaded the `next-bench` package. It prints the published
`app.dylib` and fails with the daemon log if anything prevents the build, so
the fixture is never hand-built and a Perry pin bump regenerates it instead of
rotting. `cargo test -p coop-worker --test binary_http_roundtrip` calls the
same script automatically whenever no published package matches the pinned
providers.

The generated `target/next-benchmark/coop-run/runtime.toml` keeps the
benchmark port (127.0.0.1:4580) so the measurement harness can serve this
fixture unchanged; the build itself uses an ephemeral port through
`prepare.toml`.

A narrowed standalone library, outside Coop's pipeline, is still useful when
bisecting Perry itself:

```sh
cd benchmarks/next-small
../../.perry-main/target/perry-dev/perry compile \
  --no-auto-optimize --output-type dylib \
  -o ../../target/next-benchmark/coop-next-route-direct.dylib \
  coop/coop-handler.ts
```

The common measurement harness is
`benchmarks/server-benchmark.mjs`; it validates runtime, iteration count, and
checksum on every response, records the machine load with each sample, and
reports process-tree PSS and private-dirty memory alongside RSS on Linux. Set
`COOP_BENCH_CGROUP_ROOT` (or pass `--cgroup-root`) to put the complete server
topology into a fresh child cgroup before executable startup and emit ready,
warm, post-workload, and peak cgroup memory. The harness refuses missing
`memory`, `cpu`, or `pids` controllers instead of silently weakening the
measurement.

The equivalent celld source and Wrangler manifest live under
`benchmarks/celld-small`. The local measurement deployed that Worker to the
pinned celld build and used a task-local MinIO bucket. This is suitable for a
mechanism benchmark, not a production durability benchmark. The complete
repeatable path is `scripts/run-celld-mechanism-benchmark.sh`; it verifies the
locked celld commit and esbuild version, pins both development object-store
images by digest, deploys once before timing, gives each trial fresh local
state, validates every response, and uses the common Linux PSS/cgroup
accounting. Build the locked celld binary first, then run:

```sh
export COOP_DELEGATED_CGROUP_ROOT=/sys/fs/cgroup/coop-delegated
export COOP_BENCH_CGROUP_ROOT=$COOP_DELEGATED_CGROUP_ROOT/benchmarks
CELLD_ESBUILD=/absolute/path/to/esbuild-0.28.0 \
COOP_BENCH_TRIALS=5 \
COOP_BENCH_REQUESTS=20000 \
scripts/run-in-delegated-cgroup.sh \
  scripts/run-celld-mechanism-benchmark.sh
```

The MinIO process remains an explicitly external development dependency and
is not charged to celld. A production-durability comparison must use a celld-
supported conditional-write object store and report that service separately.

## Isolated Linux comparison

The provider packaging and both tiny-app resource harnesses now support Linux
shared objects. The Linux output includes proportional set size (PSS) and
private-dirty memory as well as RSS, so shared mappings are not counted once per
Node process and application-owned writable pages remain visible. Both values
come from the same `smaps_rollup` sample. When
`COOP_BENCH_CGROUP_ROOT` names a writable delegated cgroup-v2 subtree, each
trial creates one child cgroup containing its complete server topology and
reports ready, warm, post-workload, and peak cgroup memory. The process is
moved before `exec`, so runtime initialization is accounted for. On the target
server, build once, retain the exact artifacts across trials, and run the same
1/10/100 app and fixed-request matrix:

```sh
scripts/sync-perry-main.sh
scripts/build-perry-libraries.sh
cargo build --release -p coop-daemon -p coop-worker
scripts/prepare-resource-benchmark.sh
metadata="$(mktemp)"
scripts/capture-linux-benchmark-environment.sh > "$metadata"
mv "$metadata" benchmarks/results/linux-benchmark-environment.txt

# Must be an empty, writable cgroup-v2 subtree delegated by the service
# manager or benchmark host. Its parent must enable memory, cpu, and pids.
# The launcher first places the complete Cargo/harness topology inside that
# subtree; this is required when the original session cgroup is a sibling.
export COOP_DELEGATED_CGROUP_ROOT=/sys/fs/cgroup/coop-delegated
export COOP_BENCH_CGROUP_ROOT=$COOP_DELEGATED_CGROUP_ROOT/benchmarks

COOP_BENCH_APP_COUNTS=1,10,100 \
COOP_BENCH_TRIALS=5 \
COOP_BENCH_REQUESTS=20000 \
scripts/run-in-delegated-cgroup.sh \
  cargo test --release -p coop-daemon --test resource_benchmark \
  measure_in_process_startup_and_rss -- --ignored --nocapture

COOP_BENCH_TRIALS=5 \
COOP_BENCH_REQUESTS=20000 \
COOP_BENCH_NODE_SCENARIOS=1x1,1x10,1x100,10x1,100x1 \
scripts/run-in-delegated-cgroup.sh \
  cargo test --release -p coop-daemon --test node_resource_benchmark \
  measure_node_process_startup_and_rss -- --ignored --nocapture
```

The environment capture records kernel/OS/CPU/memory/swap, current load,
cgroup membership and delegation, Rust/LLVM/Node/container toolchains, lockfile
hashes, clean Perry/celld revisions, provider and server binary hashes, and a
content hash of the complete source workspace. CI uploads it beside the raw
Perry and Node output.

The Perry result reports first activation of new files separately from restart
over the deployed files. Compare the latter with Node startup; neither may push
binding or initialization into the first request. The harness reports
`memory.current` and `memory.peak` from the scenario cgroup alongside process
RSS, PSS, private dirty, and CPU time. Record the kernel, CPU model, governor,
and cgroup delegation with the raw results. The same host, power state, load,
request count, concurrency, and response validation must be used for celld.

### Retained 2026-08-14 engineering run

The first native run used an Ubuntu 24.04 arm64 VM with six logical CPUs,
12 GiB RAM, no swap, cgroup v2, Perry `564c56308d221a51b50308d9165578fbb176e877`,
celld `3f22aedd1ea4d413b93e84afb1ce385f04be84f1`, and Node 18.19.1. Each row is the
conservative upper median of two trials with 2,000 validated requests at
concurrency 50. This closes native packaging and measurement bring-up; it does
not replace the required five-trial release run on the production Node version.

| Tiny `ok` shape | Restart/listener start | Warm PSS | Warm private dirty | Server CPU/request |
|---|---:|---:|---:|---:|
| Perry, 1 app | 702 ms | 23.6 MiB | 4.5 MiB | 90 us |
| Perry, 10 apps | 811 ms | 31.0 MiB | 11.9 MiB | 115 us |
| Perry, 100 apps | 1,442 ms | 99.3 MiB | 80.2 MiB | 95 us |
| Node, 1 process / 1 app | 548 ms | 46.7 MiB | 13.8 MiB | 300 us |
| Node, 1 process / 10 apps | 308 ms | 46.8 MiB | 13.9 MiB | 265 us |
| Node, 1 process / 100 apps | 442 ms | 47.7 MiB | 14.8 MiB | 235 us |
| Node, 10 processes / 10 apps | 1,129 ms | 172.5 MiB | 137.8 MiB | 655 us |
| Node, 100 processes / 100 apps | 8,061 ms | 1,407.5 MiB | 1,366.4 MiB | 1,800 us |

The Linux result makes the tradeoff concrete. Perry wins CPU and the one- and
ten-app consolidated-memory rows, and it decisively wins the process-isolated
Node comparison. Consolidated Node wins 100-app warm memory by 51.6 MiB. Perry's
per-app executor, thread-local runtime state, and native arena pages are now the
primary density target; that losing row must not be hidden by aggregate RSS.

The cgroup current values for the small Perry cases are lower than PSS because
Linux charges a shared file-backed page to one cgroup rather than
proportionally distributing it. Provider pages were already resident and
charged elsewhere on this VM. PSS and private dirty are therefore the primary
cross-shape memory comparison; cgroup current/peak remains an operational
capacity signal when the complete host lifecycle is measured in a fresh parent
cgroup.

### Equivalent Worker mechanism matrix

The corrected five-trial run executes the same
`/api/benchmark?iterations=100` oracle in Perry, plain Node, and celld. Every
response must contain the expected runtime label, iteration count, and checksum
`3726872593`. Trials use 20,000 requests at concurrency 50, process CPU from
`/proc`, and a separately verified cgroup for each server. Compilation,
deployment, and Perry's eager activation prime are outside the timed restart.

| Shape | Listener start | Usable cold | Warm PSS | Warm private dirty | CPU/request | p50 | p99 |
|---|---:|---:|---:|---:|---:|---:|---:|
| Perry, root-owned immutable providers | 47.3 ms | 55.6 ms | **28.24 MiB** | **6.50 MiB** | 118.5 us | 3.062 ms | 9.649 ms |
| Plain Node | 141.8 ms | 147.8 ms | 30.14 MiB | 13.79 MiB | **87.5 us** | 3.355 ms | 11.198 ms |
| celld | **20.6 ms** | **23.0 ms** | 31.45 MiB | 10.97 MiB | 117.0 us | **2.464 ms** | **8.311 ms** |

Perry wins warm memory and beats Node usable startup by 2.7x. celld wins
startup. Node wins CPU/request; Perry and celld are within 1.5 us/request in
this run, which is too close to rank confidently on this shared VM. Perry's
median throughput was 14,140 requests/s, between Node's 12,433 and celld's
16,805, illustrating why CPU, throughput, and tail latency must all be retained
rather than collapsed into one score. celld activates its cell on the first
request, so listener-ready and usable-cold remain separate columns. MinIO was
an external development dependency and was not charged to celld.

Provider verification was the dominant Perry restart cost. The original
portable policy serially hashed both 58.5 MB and 36.2 MB provider files and
produced a 565.7 ms usable-cold median. Hashing the independent files in
parallel preserved byte integrity and reduced a later median to 423.4 ms. The
production-oriented mode installs a fully hashed package under
`/opt/coop/providers/<manifest-sha256>` with root ownership, `0555` directory,
and `0444` files. An unprivileged Perry process then proves every canonical
ancestor and file is root-owned and not writable before size/identity checks;
manifest verification took 0.04 ms in the prime log and the provider pair took
5.67 ms through load and symbol binding. The worker rejects this mode for a
writable development package. `full_hash` remains the portable default.

Raw evidence and exact identities are retained in:

- `benchmarks/results/2026-08-14-linux-environment.txt`
- `benchmarks/results/2026-08-14-linux-perry-resource.txt`
- `benchmarks/results/2026-08-14-linux-node-resource.txt`
- `benchmarks/results/2026-08-14-linux-celld-resource.txt`
- `benchmarks/results/2026-08-14-linux-worker-mechanism.txt`
- `benchmarks/results/2026-08-14-linux-worker-mechanism-parallel-hash.txt`
- `benchmarks/results/2026-08-14-linux-worker-mechanism-optimized.txt`
- `benchmarks/results/2026-08-14-linux-worker-mechanism-optimized-environment.txt`

## Next engineering gates

Before claiming full Next support or testing 100 Next apps, the following need
to pass without compatibility fallbacks:

1. imported functions must reliably retain request arguments;
2. `NextRequest.nextUrl.searchParams` must work;
3. returned `Response` headers and streams must cross module boundaries;
4. Next's AsyncLocalStorage request/work stores must run;
5. the production webpack lazy route module must not deadlock or lose values;
6. first activation of many newly created macOS images must be improved or
   treated explicitly as deployment work; restart of existing images is
   already sub-second and remains fully eager;
7. the array-growth GC forwarding warning seen on every preload must be fixed.

Only then is the meaningful next matrix one, ten, and one hundred real Next
deployments on an isolated Linux host, compared with both one Node process per
deployment and any defensible shared-Node topology.
