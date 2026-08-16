# Architecture

Coop runs three tiers, split by failure boundary rather than by layer.

| tier | process | lifetime | owns |
|---|---|---|---|
| **daemon** (`coop-daemon`) | one per box | the box | TLS, routing, deployments, artifacts, admin API, metrics |
| **worker** (`coop-worker`) | one per deployment | the deployment | the app dylib, its Perry runtime state, cron, queue polling |
| **invocation** | Tokio task | one request | a single HTTP request |

## Why this split

**The daemon never runs application code.** It terminates TLS, picks a
deployment by host and path, and forwards a request over a Unix domain socket.
An application cannot take the box down by panicking, exhausting its heap, or
looping — it takes down its own worker.

**A worker owns exactly one deployment.** The Perry runtime is not designed for
multiple mutually-distrusting applications inside one address space: it has a
per-thread arena, a garbage collector with process-global side tables, and a
stdlib that assumes a single application identity. One deployment per worker
keeps that assumption true.

**An invocation is a task, not a process.** Requests within a deployment share an
address space, which is what makes per-request cost low. That is also the reason
deployments do not share one.

## Request path

```
client ──TLS──▶ coop-daemon ──unix socket──▶ coop-worker ──COOP──▶ app.dylib
                     │                              │
                router.rs                      plugin_host.rs
             host + path match              dlopen'd, symbol-pinned
```

The daemon↔worker protocol is length-prefixed JSON (`u32` big-endian length,
then the payload) carrying `WorkerRequest`/`WorkerResponse`. The worker↔app
protocol is `COOP`, a compact binary frame — see [The host ABI](host-abi.md).

Both live in `crates/coop-host-abi` so the daemon, the worker, and any test
harness share one vocabulary rather than three drifting copies.

## Isolation model

Coop treats **the compiler as the primary isolation boundary**, with the
operating system as the backstop. That ordering is unusual and worth stating
plainly, because it determines what Coop can and cannot promise.

What the compiler enforces statically:

- The TypeScript subset excludes `eval`, dynamic `Function` construction, and
  arbitrary native modules, so an application cannot reach outside the surface
  Perry compiled for it.
- Imports are restricted to an allowlist; there is no route from application
  code to raw syscalls except through the runtime.

What the compiler cannot enforce, and the OS therefore must:

- Memory exhaustion, CPU monopolisation, and runaway allocation. These are
  bounded per worker by cgroup limits on Linux (`crates/coop-daemon/src/cgroup.rs`).
- Bugs in the runtime itself. A memory-safety defect in `perry-runtime` is not
  contained by anything in Coop; it is contained by the worker being a separate
  process.

The consequence: **Coop's isolation is only as strong as Perry's soundness plus
the process boundary.** It is suitable for hosting your own portfolio. Treating
it as a sandbox for untrusted third-party code would require a threat model that
Coop does not currently claim — see `coop-spec-v0.md` for the intended
direction.

## Deployment lifecycle

1. A directory with `coop.toml` is staged into the deployment root.
2. The daemon compiles it with Perry into an application dylib, records a
   manifest (Perry commit, provider hashes, target, artifact `sha256`, size), and
   publishes it as an immutable, content-addressed package.
3. The worker verifies the manifest against the runtime it has loaded, `dlopen`s
   the image, and preloads it on a dedicated Perry thread.
4. Requests are served. Rollback re-points at a previous published package; it
   does not recompile.

The verification in step 3 is not advisory. A mismatch refuses the load. See
[Shared runtime providers](providers.md).
