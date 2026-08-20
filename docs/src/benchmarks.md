# Benchmarks

Coop's premise is memory density and cheap marginal deployments. Whether it
delivers on **per-request cost** is a separate question, and the honest current
answer is: not yet, against Node.

This page states what has actually been measured and what each number is worth.
`BENCHMARKS.md` in the repository root holds the longer engineering write-ups.

## Read this before quoting any figure

Coop's earlier published comparison used a benchmark adapter that **skipped most
of the framework**. It constructed a `NextRequest`, invoked the route function,
ran `NextResponse.json`, and then emitted a fixed payload directly — it did not
run Next's private `AppRouteRouteModule.handle`, AsyncLocalStorage work stores,
the production webpack route loader, query parsing, or response stream
extraction.

Those omissions favour Perry. The document says so itself: *"the row cannot
support a full-Next performance claim."* Any figure showing Coop ahead of Node
on CPU or RSS that predates a verified full-handler run should be read as a
**lower bound on Perry's cost**, not a result.

## The first like-for-like measurement

Once the production App Route actually ran end to end — 21 verified requests
through the real handler, with an anti-bypass assertion confirming the handler
was entered — the comparison inverted.

Single run, same `perry-host.js` entry point, same 21-request verifier, both
rows `verify=ok` and bypass-guard silent, machine load 22 at both ends:

| shape | startup | usable cold | ready RSS | warm RSS | post-load RSS | CPU/req | req/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| Perry single binary | 258 ms | 568 ms | 70.1 MiB | 161.8 MiB | 270.7 MiB | 4,595 µs | 114 |
| Node standalone | 148 ms | 299 ms | 83.1 MiB | 92.3 MiB | 127.3 MiB | 643 µs | 221 |

On identical verified work: roughly **7× the CPU per request** and **2× the
post-load RSS**, at about **half** the throughput.

Perry does win **ready RSS** — the static footprint is genuinely smaller. It is
the working set under load that inverts.

### Caveats that matter

- **N = 1.** No repetition, no medians, no variance. The direction is almost
  certainly right; the multiplier is not a settled figure.
- 420 requests per shape, one process each.
- `PERRY_RS4GC=0` — the shadow-frame GC lowering, not the aarch64 default.
- Perry's RSS was still climbing (161.8 → 270.7 MiB), so it may not have reached
  steady state.
- The **coop-hosted** row is missing: that shape died under sustained load with
  a GC-rooting signature, on artifacts predating the fix for it. Until it is
  re-measured, Coop's own hosting overhead is unquantified.


## The Linux shared-runtime measurement

The first end-to-end run of the Linux proof — providers built once, an app-only
dylib loaded against them, under real cgroup limits — produced density and
throughput figures for 1, 10 and 100 applications.

AMD EPYC 7763, 2 vCPU, 8 GB, Ubuntu 24.04, cgroup2. Medians of 2 trials, 2000
requests per trial.

| shape | ready PSS | req/s | CPU µs/req |
|---|---:|---:|---:|
| coop, 1 app | 40.9 MB | 21,293 | 45 |
| coop, 10 apps | 223.8 MB | 17,122 | 60 |
| coop, 100 apps | 374.0 MB | 15,790 | 65 |
| node, 1 proc × 100 logical apps *(no isolation)* | 52.0 MB | 5,519 | 195 |
| node, 10 procs × 10 apps | 168.4 MB | 4,138 | 345 |
| node, 100 procs × 100 apps | 1,321.2 MB | 1,341 | 630 |

Against **isolated** Node at 100 applications: about **3.5× denser** and **11.8×
the throughput**, with per-request CPU roughly **4.5× cheaper** across the range.

### Why this does not contradict the 7× figure above

It measures a different thing in four respects at once, and any one of them
would be enough to break comparability:

- **Workload.** A tiny handler here; a full Next.js App Route there.
- **Platform.** Linux x86-64 here; macOS arm64 there.
- **Hosting shape.** Coop with shared providers here; a single Perry binary
  there.
- **GC lowering.** The default here; `PERRY_RS4GC=0` there.

So the honest reading is that Perry is cheap per request on small work and has
not yet been measured cheaply on framework-sized work. The Next.js fixture
exists to close exactly that gap, and until it reports, neither number
describes "Perry versus Node".

### One result worth explaining rather than rounding off

Marginal cost per application is not linear. Going 1 → 10 apps costs about
**20 MB each**; going 10 → 100 costs about **1.7 MB each**. Coop is also *worse*
than isolated Node at 10 applications (223.8 MB against 168.4 MB) while being
far better at 100.

That shape suggests a fixed pool being amortised rather than a per-application
cost, but it has not been investigated and should not be presented as a
finding. It is the most interesting open question in this data.

### Caveats

- **2 trials**, on a 2-vCPU shared runner at load ≈ 2. Density figures are
  robust to that; latency and CPU less so.
- The tiny fixture shares almost everything through the providers, so its
  marginal cost sits near the floor **by construction**. Do not read it as
  representative of a real application.

## Density

The separate density matrix — the actual premise — is more favourable and also
more preliminary: Perry used less warm PSS than consolidated Node at one and ten
small applications, more at one hundred because of per-application executor and
runtime state, and remained dramatically denser than one Node process per
application.

That, rather than per-request CPU, is the case for the architecture today.

## Methodology notes

Anyone re-running these should know:

- **Verify correctness per row.** Three benchmark runs during this work produced
  plausible-looking numbers that were measuring failed requests. The tell was the
  Node oracle failing its own verifier. A CPU figure attached to a failed
  assertion is worse than no figure.
- **Check the load average at both ends.** An earlier three-trial rerun was
  invalidated by load 79–81 against a 39–43 baseline, and its own author said so.
- **Prefer CPU-seconds and RSS over wall time** on a shared box; those are
  load-independent.
- **Sample RSS off the request path**, not during it.
