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
| `secrets` | **implemented** — reads `PERCH_SECRET_<NAME>`, throws if unset |
| `queue` | **implemented** — host-owned durable queue via `js_perch_queue_enqueue` |
| `perchFetch` | **implemented** — policy-wrapped outbound `fetch` |
| `db` | **implemented** — connects via `PERCH_DB_URL` |
| `kv` | **stub** — logs the intended operation and returns `null`; not wired to Redis |
| `storage` | **stub** — logs the intended operation; not wired to a backend |

The stubs are honest no-ops, not silent failures: they emit a JSON line naming
the operation. Code written against them will run and do nothing.

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

`perch-worker` decrypts the deployment's secrets file at startup and exports each
as `PERCH_SECRET_<NAME>`. `get` throws a descriptive error if the secret is not
configured, rather than returning `undefined` — a missing credential should fail
loudly at first use.

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

## `kv` and `storage` (stubs)

The intended shapes are:

```ts
await kv.set("session:abc", JSON.stringify(data), { ex: 3600 });
const value = await kv.get("session:abc");   // currently always null
await kv.del("session:abc");

await storage.put(key, bytes, { contentType });
await storage.get(key);
await storage.del(key);
```

`kv` prefixes every key with `PERCH_REDIS_PREFIX` so deployments cannot collide —
that part is real, and will keep working when the Redis binding lands.
