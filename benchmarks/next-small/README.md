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
