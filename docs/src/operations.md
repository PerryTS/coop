# Deploying and rolling back

## The CLI

`perch-cli` talks to a daemon's admin API. It defaults to
`http://127.0.0.1:80`; override with the global base-URL flag.

| command | what it does |
|---|---|
| `list` | show all deployments and their status |
| `deploy <dir> <target>` | deploy a local directory to a box |
| `logs <name>` | tail logs for a deployment |
| `rollback <name>` | roll a deployment back to its previous version |
| `dev <dir>` | run a deployment locally |

`<target>` is a remote in `root@box:/var/lib/perch/deployments/<name>` form.

## What a deploy does

1. The directory is staged into the deployment root.
2. The daemon compiles it with Perry into an application dylib.
3. A manifest is recorded — Perry commit, provider hashes, target, artifact
   `sha256` and size — and the result is published as an immutable,
   content-addressed package.
4. The worker verifies that manifest against its loaded runtime, `dlopen`s the
   image, and preloads it on a dedicated Perry thread.

Step 4 refuses on mismatch. See [Shared runtime providers](providers.md).

## Rollback

`rollback` re-points at a previously published package. It does **not**
recompile, which is what makes it fast and predictable — the artifact being
rolled back to is byte-identical to the one that was serving before.

## Artifacts

Published packages are content-addressed and immutable. The daemon verifies a
package's digest before mapping it; a tampered or truncated artifact is refused
rather than loaded. Old packages are retained so rollback has somewhere to go.

## Observability

- **Metrics**: the daemon exposes Prometheus metrics; sampling is deliberately
  kept off the request path.
- **Rules and dashboard**: `ops/` contains Prometheus alerting rules, a Grafana
  dashboard, and a smoke-validation script.
- **Logs**: structured lines from `log.*` in application code, scoped by
  deployment, tailable via `perch-cli logs`.

## Resource limits

On Linux, workers run under cgroup limits (`crates/perch-daemon/src/cgroup.rs`).
That is the backstop for the things the compiler cannot prevent: memory
exhaustion and CPU monopolisation. On macOS, development is supported but those
limits are not.

## Upgrading Perry

Bumping the pinned Perry commit is a **box-wide** operation, because the
providers are shared:

1. Update `perry-main.lock` to the new commit.
2. Rebuild the providers.
3. Recompile every deployment.

There is no partial state where some deployments run against the old runtime and
some against the new. That is a deliberate consequence of the shared-provider
model, and the identity guard enforces it rather than letting it drift.
