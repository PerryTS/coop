# The `@perch/runtime` API

Handlers import from `@perch/runtime` (`packages/perch-runtime`).

```ts
import {
  PerchRequest, respond, redirect, jsonResponse, withCacheHeaders,
  db, kv, storage, queue, secrets, log, perchFetch,
} from "@perch/runtime";
```

## Implementation status

Not all of this surface is wired to a backend yet. Check here before designing
around a module:

| module | status |
|---|---|
| `log` | **implemented** — structured lines to the worker's log stream |
| `secrets` | **module implemented** — reads `PERCH_SECRET_<NAME>`, throws if unset. *Nothing in the host exports those variables yet*: `[capabilities] secrets` parses in `perch.toml` but is never read, and there is no secrets file format. |
| `queue` | **implemented** — host-owned durable queue via `js_perch_queue_enqueue` |
| `perchFetch` | **module implemented** — policy-wrapped outbound `fetch`. *Nothing in the host exports `PERCH_FETCH_ALLOWLIST` yet*, so the allowlist is empty and every domain is permitted. |
| `db` | **module implemented** — connects via `PERCH_DB_URL`. *Nothing in the host exports that variable yet*; a deployment must get it into the worker's environment itself. |
| `kv` | **implemented** — Redis via Perry's `ioredis`, per-deployment key prefix |
| `storage` | **implemented** — files under a per-deployment directory |

`kv` and `storage` are **only available to a deployment with its own worker
process** (`isolation.class = "dedicated"`). Both are scoped by environment
variables the worker exports, and `in_process` / `sharded` deployments share one
process — so a shared namespace there would be a data-isolation break, not a
degraded feature. In those modes the variables are absent and both modules throw
a message saying so, rather than silently sharing a keyspace or a directory.

## Request and response

```ts
const req = new PerchRequest(reqJson);

req.method            // string
req.path              // string
req.text()            // string
req.json()            // any
req.formData()        // Record<string, string>
req.header(name)      // string | undefined
req.queryParam(name)  // string | undefined
req.ip()              // string
```

```ts
respond(status, headers, body)      // string
jsonResponse(status, value)         // string
redirect(location, status?)         // string
withCacheHeaders(response, seconds) // string
```

A handler returns the string these produce:

```ts
export function handle(reqJson: string): string {
  const req = new PerchRequest(reqJson);
  return jsonResponse(200, { path: req.path });
}
```

## `log`

```ts
log.debug(msg, fields?);
log.info(msg, fields?);
log.warn(msg, fields?);
log.error(msg, fields?);
```

`fields` is a `Record<string, any>` merged into the structured line.

## `secrets`

```ts
const token = secrets.get("POSTMARK_TOKEN");
```

`get` throws a descriptive error if the secret is not configured, rather than
returning `undefined` — a missing credential should fail loudly at first use.

The intended host side is that `perch-worker` decrypts the deployment's secrets
file at startup and exports each as `PERCH_SECRET_<NAME>`. **That part does not
exist yet.** `[capabilities] secrets` parses in `perch.toml` and is never read,
there is no secrets file format, and no code anywhere sets a `PERCH_SECRET_*`
variable — so today `secrets.get` throws for every name unless something outside
Perch put the variable in the worker's environment.

## `db`

A query builder over the connection named by `PERCH_DB_URL`:

```ts
const rows = db.table("subscribers")
  .select("id", "email")
  .where({ active: true })
  .orderBy("created_at", "DESC")
  .limit(10);
```

Available builder methods: `table`, `select`, `where`, `join`, `groupBy`,
`orderBy`, `limit`, `offset`.

Nothing in the daemon or the worker exports `PERCH_DB_URL` today — unlike
`PERCH_REDIS_URL` and `PERCH_STORAGE_DIR`, which `perch-worker` derives and
exports for a dedicated worker. Until it does, the variable has to reach the
worker's environment some other way, or `db` throws on first use.

## `queue`

```ts
await queue.send("email", { to: "user@example.com", subject: "Welcome" });
await queue.send("email", payload, { delay: 60_000 });
await queue.sendRaw("binary", Buffer.from([0, 255]));
```

Enqueueing is host-owned: it calls into the worker rather than talking to a
broker from application code, so the queue survives the application being
recompiled or rolled back.

## `perchFetch`

A wrapper around `fetch` that applies the deployment's outbound policy. Use it
instead of bare `fetch` so that egress stays attributable to a deployment.

The allowlist comes from `PERCH_FETCH_ALLOWLIST`, which nothing exports yet, and
an empty allowlist permits every domain. `[capabilities.fetch.allowlist]` parses
in `perch.toml` but is not read. Treat this as retry and timeout handling today,
not as an egress control.

## `kv`

A Redis key-value store, backed by Perry's `ioredis` binding.

```ts
await kv.set("session:abc", JSON.stringify(data), { ex: 3600 });
const value = await kv.get("session:abc");     // string | null
const count = await kv.incr("rate_limit:ip:1.2.3.4");
const removed = await kv.del("session:abc");   // number of keys removed
```

Configure it with `[redis] url` in `runtime.toml`. The daemon passes that URL to
the worker through the environment, so a password never reaches the process
table.

`kv` prefixes every key with `PERCH_REDIS_PREFIX`, which the worker derives from
the deployment name as `perch:<name>:`. Deployments cannot address each other's
keys because the prefix is injected by the host, not the application. The worker
refuses to start a deployment whose name contains `:`, because that would make
the prefix ambiguous — deployment `a` writing the literal key `b:x` would
otherwise land exactly where deployment `a:b` writes `x`.

Three behaviours follow from what Perry's `ioredis` binding can actually lower,
and are worth knowing before you design around `kv`:

- **`{ ex }` is not atomic.** Perry's `set` takes exactly two arguments, so
  `SET ... EX` cannot be expressed and `setex` has no dispatch entry either. A
  TTL is issued as `SET` followed by `EXPIRE`. There is a window in which the
  key exists without its expiry; a crash inside it leaves a key that never
  expires. `set` checks the `EXPIRE` reply and throws if it did not apply.
- **`ex` must be a whole number of seconds ≥ 1.** `EXPIRE` has one-second
  resolution, so a fractional value is rejected rather than truncated.
- **The surface is `get` / `set` / `del` / `incr` and nothing else.** Perry
  resolves Redis methods through a fixed compile-time table; a method with no
  entry — `ttl`, `mget`, `keys`, `scan`, `setex`, anything list/set/hash — does
  not fail to compile, it evaluates to `undefined`. Adding one to `kv.ts`
  without checking that table would produce a silent no-op.

## `storage`

An S3-shaped object store, files on disk in v0.

```ts
await storage.put("uploads/avatar.jpg", imageBytes, { contentType: "image/jpeg" });
const bytes = await storage.get("uploads/avatar.jpg");   // Buffer | null
await storage.del("uploads/avatar.jpg");
const keys = await storage.list({ prefix: "uploads/", limit: 100 });
```

Configure the box-wide root with `[paths] storage_dir` in `runtime.toml`. The
worker creates and exports `<storage_dir>/<deployment>` as `PERCH_STORAGE_DIR`;
the application never sees the root, only keys under its own directory.

Layout inside that directory:

```
objects/<key>        the bytes, verbatim
meta/<key>.json      {"contentType": "..."} when one was given
```

- **`get` returns a `Buffer`, not a string.** Perry's `readFileSync(path,
  "utf8")` is lossy above `0x80`, so a string return could not carry the binary
  payloads this interface exists for. Call `bytes.toString("utf8")` when the
  object is known to be text. `put` accepts either a `Buffer` or a string.
- **Keys are rejected, not sanitised.** A key must be relative, non-empty, and
  free of `.`/`..`/empty segments, backslashes and NUL. Rewriting
  `../../etc/passwd` into something storable would put the object somewhere the
  caller did not ask for, so it throws instead.
- **`put` verifies its own write.** Perry's `fs.writeFileSync` returns without
  throwing when the write fails, so a full disk or a permissions error would
  otherwise look like a stored object. `put` stats the result and compares byte
  counts.
- **`contentType` is recorded but not yet read.** v0 has no `head()` and no
  storage-backed serving path; storing it means neither has to guess later.
  Re-putting without one clears the previous value.
- **`del` on an absent key succeeds**, matching S3.
