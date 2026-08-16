# Status: what is real

This page exists because `perch-spec-v0.md` describes an intended end state, and
it would be easy to read it as a feature list. This is the current reality.

Last reviewed against the tree at the time of writing; if you are unsure, the
code is authoritative and this page is not.

## Working

- **Compilation and serving.** TypeScript → Perry → app-only dylib → HTTP, with
  host+path routing and static file mounts.
- **Shared runtime providers.** Runtime and stdlib built once per box; each
  application resolves against that ABI. The identity guard refuses mismatched
  images rather than loading them.
- **Three-tier process model.** Daemon, per-deployment worker, per-request task.
  A worker crash does not take the box down.
- **Content-addressed artifacts** with manifest verification, and rollback that
  re-points rather than recompiles.
- **Cron and a durable queue**, owned by the worker.
- **cgroup limits** on Linux.
- **Observability**: Prometheus metrics, alerting rules, a Grafana dashboard,
  smoke validation.
- **Developer CLI**: `list`, `deploy`, `logs`, `rollback`, `dev`.

## Partial

- **`@perch/runtime`.** `log`, `secrets`, `queue`, `perchFetch` and `db` are
  wired. **`kv` and `storage` are stubs** — they log the intended operation and
  return nothing. See [the API page](runtime-api.md).
- **`perch.toml` surface.** The keys documented in
  [the reference](perch-toml.md) are what deployments use. The spec describes
  further sections (capabilities, secrets allowlists, cron and queue bindings in
  config) whose machinery partly exists in the worker but which are not all
  reachable from `perch.toml` today.
- **Framework support.** A production Next.js App Route compiles and serves. That
  is a real demonstration, not a general guarantee — see
  [the subset page](subset.md).

## Not yet

- **Performance parity with Node** on per-request CPU. Currently around 7× worse
  on like-for-like work; see [Benchmarks](benchmarks.md).
- **Multi-tenant isolation for untrusted code.** The architecture is designed
  toward it, but the current threat model assumes you own the code you deploy.
  See the isolation section of [Architecture](architecture.md).
- **Multi-version runtimes on one box.** The shared-provider model means one
  Perry version per machine, by construction.

## Known operational sharp edges

- **The version string is not a freshness signal.** Perry's `0.5.1510` spanned
  more than a dozen commits including significant runtime changes. Pin and
  compare commits.
- **`cargo test` needs `--no-fail-fast`.** Without it the run stops at the first
  failing target and silently skips three worker test binaries.
- **A skipped suite looks like a passing one.** Skips are early `return`s. Re-run
  with `--nocapture` and grep for `skip:` if you need to know a suite ran.
- **Hand-built fixtures go stale on every Perry bump** and then fail the identity
  guard. Prefer fixtures the daemon can regenerate from source.

## Contributing to this page

If you implement something listed under *Partial* or *Not yet*, move it — and if
you find something under *Working* that isn't, move it the other way. A status
page that overstates is worse than none, because it gets trusted.
