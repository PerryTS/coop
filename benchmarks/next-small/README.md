# Small Next server benchmark

This is a production-buildable Next.js 16 App Route fixture for measuring the
server shape described in the repository's `BENCHMARKS.md`.

- `app/api/benchmark/route.ts` is the source used by the normal Next standalone
  build and the Perry adapter.
- `perch/perch-handler.ts` adapts Perch's synchronous wire ABI to the route.
- `npm run build` creates the unmodified production Next standalone server.

The Perry adapter is intentionally a lower bound, not full Next hosting. It
constructs public Next request/response objects and runs the same route work,
but the pinned Perry cannot yet transport all request and response values across
the module boundaries used by Next. The exact omitted behavior and measured
results are documented in `../../BENCHMARKS.md`.

Build the Node form from this directory:

```sh
npm ci
npm run build
```

The Perch form is never hand-built. `perch/perch.toml` plus `perch-handler.ts`
and `app/api/benchmark/route.ts` are the deployment's compiler inputs;
`../../scripts/prepare-next-benchmark.sh` stages them and lets the Perch daemon
compile, publish, and load the `next-bench` package. Run it directly, or let
`cargo test -p perch-worker --test binary_http_roundtrip` invoke it whenever the
published package no longer matches the pinned Perry providers.

A standalone library outside Perch's pipeline is still handy when bisecting
Perry itself:

```sh
../../.perry-main/target/perry-dev/perry compile \
  --no-auto-optimize --output-type dylib \
  -o ../../target/next-benchmark/perch-next-route-direct.dylib \
  perch/perch-handler.ts
```
