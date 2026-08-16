# Introduction

Perch runs TypeScript applications that have been compiled to native code by
[Perry](https://github.com/PerryTS/perry).

The distinguishing idea is not "compile TypeScript" — Perry does that. It is
**where the runtime lives**. A Perry program normally links the language runtime
and standard library into its own executable. Perch builds those once per box as
shared libraries and compiles each application into a small dylib that resolves
against them:

```
        one per box                          one per deployment
  ┌───────────────────────────┐        ┌───────────────────────────┐
  │ libperry_runtime_provider │◀───────│  next-app.dylib   (~4 MB) │
  │ libperry_stdlib_provider  │        │  landing.dylib    (~1 MB) │
  └───────────────────────────┘        │  api.dylib        (~2 MB) │
                                       └───────────────────────────┘
```

That makes the marginal cost of an additional application small, which is the
whole premise: run a portfolio of small services on one machine without paying
for a language runtime per service.

## What you get

- **HTTP handlers** compiled to native code, routed by host and path
- **Static file serving** alongside handlers
- **Cron** and a **durable queue**, run by the same worker that owns the app
- **Per-deployment isolation** at the process level, with cgroup limits on Linux
- An **admin API**, Prometheus metrics, and a Grafana dashboard under `ops/`

## What this is not

Perch is deliberately not a Kubernetes replacement, a multi-region platform, or
a general-purpose Node host. It targets one box, one operator, and a portfolio of
services whose combined traffic fits on that box.

More importantly for anyone evaluating it: **it does not run arbitrary npm code.**
Applications must compile under Perry's TypeScript subset. See
[The TypeScript subset](subset.md) before planning a migration.

## Where to start

- [Quick start](quickstart.md) — build the providers, run an example
- [Architecture](architecture.md) — the three tiers and why they are split that way
- [Status](status.md) — what is implemented, what is stubbed, what is aspirational

The original design document is preserved unedited at `perch-spec-v0.md` in the
repository root. It describes the intended end state; this documentation
describes what exists.
