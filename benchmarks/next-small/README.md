# Small Next server benchmark

This is a production-buildable Next.js 16 App Route fixture for measuring the
server shape described in the repository's `BENCHMARKS.md`.

- `app/api/benchmark/route.ts` is the source used by the normal Next standalone
  build and the Perry adapter.
- `coop/coop-handler.ts` adapts Coop's synchronous wire ABI to the route.
- `npm run build` creates the unmodified production Next standalone server.

The Coop adapter now drives Next's own `AppRouteRouteModule.handle` from the
production build, not the userland `GET` export. `handle` is what sets up the
AsyncLocalStorage work stores, resolves the handler for the method, applies
`fetchCache`, and builds the response; calling `GET` runs the route body and
skips all of it.

Two things this fixture previously got wrong, both of which invalidated every
number it produced:

1. It called `GET(request)`, **threw the result away**, and emitted a hardcoded
   200 with a hardcoded body. It ran the framework work and then fabricated the
   answer, so no assertion about the response could ever fail.
2. It shipped a `.next-production-bundle/` **checked into git**, which had
   drifted from `app/api/benchmark/route.ts` — the committed copy parsed
   `nextUrl.searchParams`, clamped iterations to 1..10000, set an
   `x-perch-benchmark-body` header, and was emitted by webpack while the
   current toolchain emits turbopack. Coop would have been measured against
   different code than the Node build compiles from the same source.

The bundle is now **built**, never committed, and `prepare-next-benchmark.sh`
rebuilds it whenever `route.ts` is newer. A build output living in git can only
drift again, silently, and (2) is what that costs.

Import order in `coop-handler.ts` is load-bearing: the route bundle must be
imported **before** `next/server`, because loading it installs Next's require
hook. Reverse them and `next/server` resolves to the edge build, whose module
init throws `Invariant: AsyncLocalStorage accessed in runtime where it is not
available`.


## Where this fixture is measured, and why not in CI

Compiling it peaks **above 8.3 GB RSS**. A GitHub-hosted runner has 7.75 GB, so
it cannot complete there at any cap setting — both attempts died at the
daemon's limit rather than at their true peak, which is why the first
measurement (4.2 GB) was the cap and not the peak.

Reducing Perry's codegen concurrency would fix it, but Coop deliberately calls
`env_clear()` before spawning the compiler so ambient `PERRY_*` switches cannot
silently change emitted code without changing build identity, and
`COMPILER_ENV_ALLOWLIST` carries toolchain paths only. That guard is worth more
than the convenience of overriding it.

So the Linux proof gates the tiny dependency-free fixture, and the Next steps
are behind the `next_fixture` workflow-dispatch input, off by default. Run them
on a host that can carry the compile. It has been done on an 8-core/8 GB M1
mini in about eight minutes — note that is barely more RAM than the runner, so
the deciding factor is not memory alone: macOS compresses and swaps under
pressure, while the Linux path is stopped dead by the daemon's `compile_max_rss_mb`
cap. A machine with more real memory is still the safer choice.

Verified there against the pinned Perry: a COOP request frame in,
`AppRouteRouteModule.handle` executed natively, `status: 200` and the route's
own body out.

Build the Node form from this directory:

```sh
npm ci
npm run build
```

The Coop form is never hand-built. `coop/coop.toml` plus `coop-handler.ts`
and `app/api/benchmark/route.ts` are the deployment's compiler inputs;
`../../scripts/prepare-next-benchmark.sh` stages them and lets the Coop daemon
compile, publish, and load the `next-bench` package. Run it directly, or let
`cargo test -p coop-worker --test binary_http_roundtrip` invoke it whenever the
published package no longer matches the pinned Perry providers.

A standalone library outside Coop's pipeline is still handy when bisecting
Perry itself:

```sh
../../.perry-main/target/perry-dev/perry compile \
  --no-auto-optimize --output-type dylib \
  -o ../../target/next-benchmark/coop-next-route-direct.dylib \
  coop/coop-handler.ts
```
