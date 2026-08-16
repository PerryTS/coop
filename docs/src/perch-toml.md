# `perch.toml` reference

Every deployment is a directory containing a `perch.toml` at its root.

## Minimal example

```toml
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

## Top level

| key | type | required | meaning |
|---|---|---|---|
| `name` | string | yes | Deployment identity. Used for the socket, the artifact path, log scoping, and the KV key prefix. |
| `version` | string | yes | Your version string. Recorded in the artifact manifest; Perch does not parse it semantically. |

## `[hosts]`

| key | type | meaning |
|---|---|---|
| `domains` | array of strings | Host headers routed to this deployment. |

Routing is by host first, then path. Two deployments claiming the same domain
and overlapping paths is a configuration error, not a load-balancing feature.

## `[[handlers]]`

One table per route. Repeatable.

| key | type | meaning |
|---|---|---|
| `file` | string | Path to the TypeScript handler, relative to the deployment root. |
| `path` | string | Request path to match. |
| `method` | string | HTTP method to match. |

The file must export a `handle` function. Its shape depends on which ABI you
target — see [The `@perch/runtime` API](runtime-api.md) for the string form and
[The host ABI](host-abi.md) for the binary `PCH2` form.

## `[[static]]`

One table per static mount. Repeatable.

| key | type | meaning |
|---|---|---|
| `directory` | string | Directory to serve from, relative to the deployment root. |
| `path` | string | URL prefix to mount it at. |

## A note on what is not here

The design document (`perch-spec-v0.md`) describes further sections —
capabilities declarations, secrets allowlists, cron schedules, queue bindings.
Some of the machinery behind those exists in the worker; the `perch.toml` keys
above are what deployments in this repository actually use today.

If you need a key that is documented in the spec but not listed here, check
`crates/perch-daemon/src/schema.rs` and `crates/perch-daemon/src/config.rs`
before assuming it is wired up. Documenting the spec as though it were
implemented is the failure mode this page is written to avoid.
