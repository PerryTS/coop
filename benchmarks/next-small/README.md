# Small Next server benchmark

This is a production-buildable Next.js 16 App Route fixture for measuring the
server shape described in the repository's `BENCHMARKS.md`.

- `app/api/benchmark/route.ts` is the source used by the normal Next standalone
  build and the Perry adapter.
- `perch/perch-handler.ts` adapts Perch's synchronous wire ABI to the route.
- `npm run build` creates the unmodified production Next standalone server.

The Perry adapter is intentionally a lower bound, not full Next hosting. It
constructs public Next request/response objects and runs the same route work,
but Perry 0.5.1503 cannot yet transport all request and response values across
the module boundaries used by Next. The exact omitted behavior and measured
results are documented in `../../BENCHMARKS.md`.

Build both forms from this directory:

```sh
npm ci
npm run build

../../.perry-main/target/perry-dev/perry compile \
  --no-auto-optimize --output-type dylib \
  -o ../../target/next-benchmark/perch-next-route-direct.dylib \
  perch/perch-handler.ts
```
