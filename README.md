# Coop

*A place for Perry code to rest and run.*

Coop is a single-binary runtime that hosts TypeScript applications compiled to
native code by [Perry](https://github.com/PerryTS/perry). You push a directory of
TypeScript, Coop compiles it into a native shared library and serves it over
HTTP — with the language runtime and standard library supplied **once per box**
rather than linked into every application.

```
  TypeScript  ──perry──▶  app.dylib  ──dlopen──▶  coop-worker
                             (4 MB)                    │
                                                shared providers
                                          libperry_runtime · libperry_stdlib
```

The point of that split: a hundred applications on one machine share one copy of
the runtime, and each application image is small enough to load on demand.

---

## Status

Coop is **pre-1.0 infrastructure**, developed for the Skelpo portfolio. It works,
it is tested, and it is not yet a product. Read [`docs/src/status.md`](docs/src/status.md)
for an honest account of what is implemented, what is partial, and what is
aspirational in the spec.

Two things worth knowing before you invest time:

- The **TypeScript subset** is Perry's, not TypeScript's. Some language features
  and most of npm will not compile. See [the subset guide](docs/src/subset.md).
- **Performance is not yet a settled story.** Coop's hosting model has real
  memory-density advantages, but per-request CPU is currently worse than Node on
  equivalent work. The numbers and their caveats are in
  [`docs/src/benchmarks.md`](docs/src/benchmarks.md) — including which previously
  published figures were measured against a workload that skipped most of the
  framework.

---

## Quick start

Requirements: a Perry checkout (pinned by `perry-main.lock`), the Rust nightly
named in `rust-toolchain.toml` (rustup picks it up automatically; it matches
Perry's own pin, and the two are bumped together), and Node for the developer
tooling.

```bash
# 1. Build the shared runtime + stdlib provider libraries once
./scripts/build-perry-libraries.sh

# 2. Build the daemon and worker
cargo build --release -p coop-daemon -p coop-worker -p coop-cli

# 3. Run a deployment locally
./target/release/coop-cli dev ./examples/landing
```

A minimal deployment is a directory with a `coop.toml` and at least one handler:

```
landing/
├── coop.toml
├── handlers/
│   └── contact.ts
└── static/
    └── index.html
```

```toml
# coop.toml
name = "landing"
version = "0.1.0"

[hosts]
domains = ["landing.test"]

[[handlers]]
file = "handlers/contact.ts"
path = "/contact"
method = "POST"

[[static]]
directory = "./static"
path = "/"
```

Full reference: [`docs/src/coop-toml.md`](docs/src/coop-toml.md).

---

## Writing a handler

Handlers import from `@coop/runtime`:

```ts
import { CoopRequest, respond, jsonResponse, log, db } from "@coop/runtime";

export function handle(reqJson: string): string {
  const req = new CoopRequest(reqJson);
  log.info("request received", { method: req.method, path: req.path });

  const rows = db.query("SELECT id, email FROM subscribers LIMIT 10");
  return jsonResponse(200, { subscribers: rows });
}
```

The runtime surface — `db`, `kv`, `storage`, `queue`, `secrets`, `log`,
`coopFetch` — is documented in [`docs/src/runtime-api.md`](docs/src/runtime-api.md).

There is also a lower-level binary protocol (`COOP`) for handlers that need to
avoid JSON entirely; see [`docs/src/host-abi.md`](docs/src/host-abi.md).

---

## Architecture

Three tiers, each with a different failure boundary:

| tier | process | what it owns |
|---|---|---|
| **daemon** (`coop`) | one per box | TLS, routing, deployments, artifacts, metrics |
| **worker** (`coop-worker`) | one per deployment | the app dylib, its Perry runtime state, cron, queue polling |
| **invocation** | Tokio task | a single request |

The daemon never runs application code. A worker crash takes down one
deployment, not the box.

The full design — including why the compiler is treated as the primary
isolation boundary — is in [`docs/src/architecture.md`](docs/src/architecture.md),
and the original design document is preserved at [`coop-spec-v0.md`](coop-spec-v0.md).

---

## The shared-provider model

This is what distinguishes Coop from "compile each app to its own binary".

`perry-runtime` and `perry-stdlib` are built **once** as shared libraries
(`libperry_runtime_provider.dylib` / `.so`). Each application dylib is compiled
to resolve against that ABI instead of embedding its own copy. A manifest records
the Perry commit, the provider hashes, and the target, and the worker refuses to
load an application built against a different runtime identity.

That refusal is deliberate and it will bite you during upgrades — it is the
mechanism that stops a subtly-mismatched runtime from corrupting memory at
request time. See [`docs/src/providers.md`](docs/src/providers.md) for the build,
the manifest format, and how to regenerate artifacts after a Perry bump.

---

## Repository layout

| path | what it is |
|---|---|
| `crates/coop-daemon` | the box daemon: routing, TLS, deployments, admin API |
| `crates/coop-worker` | per-deployment host: dylib loading, cron, queue |
| `crates/coop-host-abi` | shared vocabulary: daemon↔worker JSON, host↔app `COOP` |
| `crates/coop-cli` | developer CLI (`list`, `deploy`, `logs`, `rollback`, `dev`) |
| `crates/perry-stdlib-shared` | the cdylib that publishes the stdlib provider ABI |
| `packages/coop-runtime` | `@coop/runtime`, the TypeScript API handlers import |
| `scripts/` | provider builds, benchmark harnesses, fixture preparation |
| `ops/` | Prometheus rules, Grafana dashboard, smoke validation |
| `docs/` | this documentation, as an mdBook |

---

## Documentation

```bash
mdbook serve docs   # http://localhost:3000
```

Or read the sources directly under [`docs/src/`](docs/src/).

---

## Testing

```bash
cargo test --no-fail-fast
```

`--no-fail-fast` is not optional in practice: without it the run stops at the
first failing target and silently skips three worker test binaries.

Some suites self-skip when their fixtures are absent (missing providers, no
compiled Perry). A skip is an early `return`, which counts as a pass — so if you
need to know a suite actually ran, re-run with `--nocapture` and check for
`skip:` lines.

---

## License

See the repository's license file.
