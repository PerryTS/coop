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
