# Coop: Spec v0

*A place for Perry code to rest and run.*

A single-binary, single-box runtime for TypeScript workers compiled natively via Perry. Designed as infrastructure for the Skelpo portfolio first, with the architectural decisions chosen so that "open it up as a hosted product" is a ~2-hour config change rather than a rewrite.

---

## What it is

One binary. One Hetzner box. You push a directory of TypeScript files, the binary compiles them with Perry into native machine code, and runs them as HTTP handlers, cron jobs, and queue workers with a built-in database, key-value store, object store, and queue — all in one process, all operationally boring.

Think: what Heroku was in 2010, if Heroku had been a single Go binary instead of a SaaS, and if the application code compiled to native instead of interpreted Ruby.

### The elevator pitch

> I got tired of running the same boring infrastructure for every small project — a Postgres, a Redis, a cron daemon, a queue worker, an nginx, a logging stack — and spending half my ops time keeping it all alive. So I built a single binary that does all of it, compiles TypeScript to native, and runs an entire portfolio of APIs on one €20 box.

### Non-goals

This is deliberately not trying to be:

- A Kubernetes replacement
- A multi-region cloud
- A venture-scale business
- An AWS competitor
- A horizontally-scaling distributed system
- Production-grade for mission-critical systems in v0

It is trying to be: **the thing that replaces seven DigitalOcean droplets and a dozen Docker containers with one binary on one box**, for the kind of small-to-medium projects that make up 95% of real software.

---

## Goals

- **Microsecond cold starts** — per-invocation setup in single-digit microseconds via linear memory pooling
- **Sub-megabyte overhead per invocation** — dramatically denser than Lambda (~50MB), Cloudflare Workers (~3MB), or Fly Machines (~5MB)
- **Native execution speed** — no JIT warmup, no GC pauses, no interpreter overhead
- **Memory-safe by construction** — the Perry compiler refuses to emit unsafe code rather than sandboxing a binary after the fact
- **Operational simplicity** — one binary, one config file, `systemctl restart coop`
- **Graceful upgrade path** — v0 runs on one box with SQLite-like simplicity; v1+ scales without rewriting any worker code

---

## Threat model

### In scope

A worker author writes TypeScript that, intentionally or accidentally, tries to:

- Read memory it doesn't own
- Corrupt the host process
- Exhaust resources (memory, CPU, stack)
- Escape into the OS
- See another deployment's data (database, filesystem, secrets)
- Crash the runtime

We defend against all of these through a combination of compile-time restrictions and OS-level process separation.

### Out of scope for v0

- Spectre-class side channels
- Timing attacks between co-located deployments
- Mutually-untrusting tenants running actively adversarial code on the same box
- Bugs in LLVM or the Coop runtime itself

### The core insight

We are not sandboxing a binary — we are refusing to compile unsafe source. Because Perry owns the TypeScript → LLVM pipeline, we can statically guarantee things at compile time that Docker and Firecracker enforce at runtime. This gives us isolation that is **stronger than Docker and weaker than Firecracker, without the overhead of either**.

---

## Isolation architecture: the compiler as hypervisor

The central claim of this spec is that controlling the compiler gives us enough isolation to avoid both Docker and actual VMs. Let's be precise about what that means.

### What the compiler guarantees statically

**Memory safety is fully compile-time.** Every load and store in worker code is emitted by the Perry backend. There is no way for worker code to construct a pointer outside its linear memory region because the compiler never emits an instruction that could do so. This is stronger than Docker (which relies on the kernel) and equivalent to a hypervisor for the memory-reading-memory threat.

**Control flow integrity is fully compile-time.** Indirect calls go through a function table built by the compiler and installed by the runtime. Worker code cannot jump to arbitrary addresses. No ROP, no JOP, no stack-smashing-to-shellcode attacks.

**System call access is fully compile-time.** Worker code has no way to invoke a syscall because the compiler refuses to emit `syscall` instructions and the TypeScript source has no primitive that could lower to one. The only way out of the sandbox is through host functions, and host functions are a finite list that we control and audit.

**Resource accounting for CPU is compile-time instrumented.** The Perry backend inserts instruction budget decrements at every loop backedge and function entry. A worker that tries to spin forever is killed in deterministic time, not by the OS scheduler.

**No ambient authority.** Worker code has no filesystem, no network, no clock, no random, no environment variables, no process list — except what is explicitly provided through host functions. The attack surface for escape is whatever is in the host function table.

### What the compiler cannot guarantee

Honesty about the gaps:

- **Resource exhaustion through amplification**: a worker calling `fetch` in a tight loop, or running an expensive database query, is a valid worker doing valid things that happens to be expensive. Defense is at the host function level — rate limiting, query cost analysis, connection pooling — not at the codegen level.
- **Microarchitectural side channels**: Spectre, cache timing attacks. Not defended.
- **Runtime bugs**: a bug in the Coop binary could be exploitable. Mitigation: write the runtime in Rust, keep the host function surface small, fuzz the boundary.
- **LLVM bugs**: a codegen bug that violates the bounds-check invariant would punch a hole in the safety story. Mitigation: keep LLVM updated, run our own codegen tests.

### The layered isolation model

Putting it together into a defensible security architecture:

**Compiler isolation between deployments within a process.** When all of Ralph's own deployments (Chirp, GSCMaster, Fascinating News) run in the same process, the compiler enforces that they can't touch each other's memory. This works because all the code comes from one trusted author.

**OS isolation between untrusted tenants.** When the hosted version has multiple users, each user gets their own OS process. The kernel enforces that user A's process cannot read user B's process memory. This is the same isolation every server has provided since 1970 and it's rock-solid.

**VMs only when the OS itself isn't trusted enough.** Not in v0, not in v1, maybe never. For mission-critical multi-tenant SaaS with active adversaries this would be the next layer, but it's explicitly out of scope.

**The rule of thumb: compiler isolation between trusted things, OS isolation between untrusted things, VMs only when the OS itself isn't trusted.** This is a defensible three-tier model and it's cheaper at every level than the alternatives.

---

## The three-tier execution model

Coop has three tiers of execution state, each with different lifetimes and isolation properties. Understanding these is critical because they determine the entire runtime shape.

### Tier 1: The daemon (`coop`)

One long-running process per box. Runs as a systemd service. Responsibilities:

- Watches the `deployments/` directory for changes
- Spawns and supervises `coop-worker` processes (one per deployment)
- Routes incoming HTTP traffic to the right worker process (via Unix sockets)
- Holds the shared Postgres and Redis connection configuration (but not the connections themselves)
- Exposes the admin UI at `/_coop/admin`
- Aggregates logs and metrics across all deployments

The daemon is not in the hot path for requests. It's a supervisor and a router. It doesn't execute worker code.

### Tier 2: Worker processes (`coop-worker`)

One OS process per deployment. Spawned by the daemon when a deployment is loaded. Responsibilities:

- Loads the deployment's compiled `.so` via `dlopen`
- Runs a Tokio multi-threaded runtime (one OS thread per core by default)
- Holds this deployment's Postgres connection pool (sized per deployment config)
- Holds this deployment's Redis connection pool
- Runs the deployment's cron scheduler and queue poller as background tasks
- Handles incoming requests routed from the daemon
- Dispatches invocations to the right worker entry point

The worker process is where isolation happens between deployments. It crashes → only that deployment is affected. It leaks memory → only that deployment's budget is consumed. It needs a restart → only that deployment is restarted.

### Tier 3: Invocations (Tokio tasks)

One Tokio task per HTTP request, cron fire, or queue message. Lives inside a worker process. Runs on the worker's shared Tokio thread pool, multiplexed with thousands of other tasks.

Not an OS thread. Not a process. A lightweight async task sharing an OS thread with many siblings. This is what makes thousands of concurrent invocations per deployment cheap.

### Why this specific hierarchy

- **Deployments need real isolation** → processes give us that for free, cheaply (single-digit MB overhead per idle deployment)
- **Workers within a deployment share state anyway** → no need to isolate them further, and putting them in one process means they can share connection pools, caches, and compiled code
- **Invocations are ephemeral and numerous** → OS threads are too expensive; Tokio tasks cost ~1KB each and we can have millions
- **Perry's native threads are for intra-worker parallelism**, not invocation isolation — a worker can spawn parallel work within its own linear memory if it needs to, but the unit of *invocation* isolation is the compiler's linear memory, not an OS thread

---

## The compilation model

### How code gets from TypeScript to running

1. **Deploy time.** A deployment directory is pushed to the box (via rsync, scp, git pull, or any other mechanism). The daemon detects it, reads `coop.toml`, enumerates the worker source files, and invokes `perry compile --target worker -o chirp.so handlers/*.ts crons/*.ts queues/*.ts`. This takes 1-5 seconds depending on the size of the deployment.

2. **Process startup.** The daemon spawns `coop-worker --deployment chirp`. The new process `dlopen`s `chirp.so`, which maps it into the process's address space as executable memory. The dynamic linker resolves exported symbols (worker entry points) and imported symbols (host functions). Setup takes a few hundred milliseconds, dominated by Tokio and connection pool init.

3. **Hot path.** An HTTP request arrives. The Tokio task looks up the right entry point in a dispatch table, grabs a linear memory region from the per-deployment pool, sets up the worker's execution context, and calls the entry point. **This is microseconds of setup, not milliseconds.**

4. **Invocation end.** The worker's `handle` returns, the task reads the response out of the worker's linear memory, `madvise(MADV_DONTNEED)`s the memory back to the pool, and sends the HTTP response to the client.

### The output format

Perry emits, per deployment, a **position-independent shared library** (`.so` on Linux, `.dylib` on macOS). The shared library contains:

- Compiled machine code for every worker entry point in the deployment
- A symbol table exporting each entry point under a known name (`__coop_handler_ingest`, `__coop_cron_daily_digest`, etc.)
- Undefined symbols for host functions (`__coop_host_fetch`, `__coop_host_db_query`, etc.), resolved by the runtime at `dlopen` time
- The Perry TypeScript runtime's implementation of collections, allocators, strings, etc., compiled in worker mode
- Metadata about declared crons, queues, capabilities, and configuration

### Why shared library rather than executable

- **Compilation is amortized across invocations** — compile once per deploy, run millions of times
- **Code is `mmap`'d read-only, shared across all invocations in the process** — no copying, no re-loading
- **Clean ABI boundary via symbols** — the runtime and the worker code never need to parse each other's bytes
- **Reloadable** — new version means new process with new `.so`; old process is drained and killed
- **Keeps the runtime in charge** — deployments are pluggable code, not standalone programs

### What the runtime refuses to do

- **Never JIT compile per-invocation.** Kills microsecond cold starts.
- **Never interpret.** Kills the native-speed story.
- **Never share a process between untrusted users.** OS isolation is the backstop for untrusted code.
- **Never spawn a process per invocation.** Lambda's old mistake. ~100ms minimum overhead on fork+exec.

---

## The TypeScript subset

Worker code is TypeScript, validated by a Perry frontend pass before any IR is generated. Rejected source produces a compile error with a line number, not a runtime mystery.

### Banned language features

- `eval`, `Function` constructor, `new Function(...)`, any dynamic code construction
- `import()` (dynamic imports)
- `require()` if Perry has CJS interop
- `with` statements
- Direct access to `globalThis`, `window`, `self`, `global`
- `Proxy`/`Reflect` against host-provided objects
- Prototype pollution: `Object.setPrototypeOf`, `__proto__` assignment, `Object.prototype` mutation
- `SharedArrayBuffer`, `Atomics` (no shared memory between invocations)
- `WebAssembly.*`
- Top-level `await` with I/O at module scope
- Top-level side effects beyond pure declarations

### Banned imports

- All Node.js built-ins: `fs`, `net`, `http`, `https`, `child_process`, `cluster`, `dgram`, `dns`, `os`, `path`, `process`, `tls`, `vm`, `worker_threads`
- All npm packages in v0 (allowlist comes later)
- All browser APIs

### Allowed imports

- `@coop/runtime` — the host function namespace (database, kv, fetch, log, etc.)
- `@perry/worker_std` — pure computation: `Array`, `Object`, `Map`, `Set`, `String`, `Number`, `Math`, restricted `Date`, `JSON`, `RegExp`, `Promise`, `TextEncoder`/`TextDecoder`
- Sibling files in the same deployment, recursively validated

### Required worker shapes

HTTP handler:
```typescript
import type { Request, Response } from "@coop/runtime";

export default async function handle(req: Request): Promise<Response> {
  // ...
}
```

Cron job:
```typescript
import type { CronContext } from "@coop/runtime";

export default async function run(ctx: CronContext): Promise<void> {
  // ...
}
```

Queue worker:
```typescript
import type { QueueMessage } from "@coop/runtime";

export default async function process<T>(msg: QueueMessage<T>): Promise<void> {
  // ...
}
```

Exactly one default export per file, exactly that signature. The compiler verifies this statically.

### Compiler-inserted instrumentation

- **Instruction budget counter** at every loop backedge and function entry, decrementing a counter in a reserved register
- **Stack depth counter** at each function entry, trapping on overflow
- **Bounds-checked memory layout** via the 4GB guard region trick (see below)
- **Indirect call table** for function pointers, closures, and method dispatch

---

## Memory isolation via LLVM codegen

The linear memory model (borrowed from Wasmtime but implemented on LLVM):

### Per-invocation memory layout

- Reserve 4GB of virtual address space per invocation at creation time (`mmap` with `PROT_NONE`)
- Commit 16MB of that as read/write (configurable; the worker's actual memory)
- Leave the other ~4GB as a `PROT_NONE` guard region
- A base pointer in a reserved register (`r15` on x86-64) points at the start of committed memory
- All heap allocations from the worker's allocator return offsets into this region
- All loads and stores are emitted as `base + offset` by the Perry LLVM backend

### The 4GB guard region trick

With offsets constrained to 32 bits and a base register holding a 64-bit pointer, the maximum reachable address is `base + 4GB`. Everything past the committed region is `PROT_NONE`, so out-of-bounds accesses fault into the guard and get caught by the SIGSEGV handler.

**Result: zero explicit bounds-check instructions on the hot path.** The MMU does the work, and only actual violations pay a cost.

### What the compiler refuses to emit

- Any load or store whose address isn't provably `base + offset` with a 32-bit offset
- Any call to an address not in the per-deployment function table
- Any inline assembly
- Any LLVM intrinsic that could touch memory outside the linear region (worker-mode `memcpy` is bounds-aware)

### Trap handling

Traps come from:
- SIGSEGV in the guard region (out-of-bounds access) — caught by a signal handler using `sigaltstack`, which determines if the fault is in a worker's guard and `siglongjmp`s back to the trampoline
- Compiler-inserted budget/depth checks — direct runtime call
- Explicit worker panics — caught by the Perry runtime's top-level handler
- Wall-clock timeouts — Tokio task cancellation

All traps funnel through the same recovery path: log the trap, release the linear memory, return a structured error to the caller. The worker process keeps running. Other in-flight invocations are unaffected.

---

## The four primitives

Workers come in exactly four shapes. If it doesn't fit one of these, it doesn't belong in Coop.

### HTTP handlers

A TypeScript file exporting `handle(req: Request): Promise<Response>`. The runtime routes requests based on path matching.

```typescript
import { db, log } from "@coop/runtime";

export default async function handle(req: Request): Promise<Response> {
  const body = await req.json();
  
  await db.table("events").insert({
    type: body.type,
    payload: body,
    created_at: Date.now(),
  });
  
  log.info("stripe event received", { type: body.type });
  return new Response("ok", { status: 200 });
}
```

### Cron jobs

A TypeScript file exporting `run(ctx: CronContext): Promise<void>` plus a cron expression in the config.

```typescript
import { db, fetch, log } from "@coop/runtime";

export default async function run(): Promise<void> {
  const feeds = await db.table("rss_feeds").where({ active: true }).all();
  
  for (const feed of feeds) {
    const resp = await fetch(feed.url);
    // ... parse and insert new items
  }
  
  log.info("rss poll complete", { feeds: feeds.length });
}
```

### Queue workers

A TypeScript file exporting `process(msg: QueueMessage<T>)` plus a queue name and concurrency.

```typescript
import { fetch, log } from "@coop/runtime";

interface EmailMessage {
  to: string;
  subject: string;
  body: string;
}

export default async function process(msg: QueueMessage<EmailMessage>) {
  const { to, subject, body } = msg.payload;
  
  await fetch("https://api.postmark.com/email", {
    method: "POST",
    headers: { "X-Postmark-Server-Token": msg.secrets.postmark },
    body: JSON.stringify({ To: to, Subject: subject, TextBody: body }),
  });
  
  log.info("email sent", { to });
}
```

### Static sites

A directory of files served at a path prefix. No code, no template engine, just bytes on disk with correct MIME types and cache headers.

---

## Storage: the managed services

Coop includes a set of built-in data services. Each is implemented as a shared instance that the runtime manages, with per-deployment namespacing enforced at the host function level.

### Postgres (not SQLite)

**Why Postgres and not SQLite:** SQLite was tempting for operational simplicity but has three problems that matter for the upgrade path — no network access (so the runtime and the database must share a filesystem), "a million files" when you scale to many deployments, and a worse story for the "move storage to its own box" step. Postgres solves all three and isn't dramatically harder to operate.

**Architecture:**

- One Postgres instance per box in v0 (managed by the daemon, or an external Postgres the daemon points at)
- Each deployment gets its own Postgres schema: `CREATE SCHEMA deployment_chirp`
- Each deployment gets a dedicated role with privileges only on that schema
- The runtime holds a connection pool per deployment, sized per config
- The worker never sees a raw connection — it uses the typed query builder via host functions

**The typed query builder:**

Worker code doesn't get raw SQL. It gets a query builder that generates parameterized SQL the runtime validates before executing.

```typescript
import { db } from "@coop/runtime";

// CRUD
const user = await db.table("users").where({ id: 42 }).first();
const all = await db.table("events").where({ type: "signup" }).limit(100).all();

await db.table("events").insert({
  type: "signup",
  user_id: 42,
});

await db.table("users").where({ id: 42 }).update({ name: "Ralph" });
await db.table("stale_sessions").where({ expires_at: { lt: Date.now() } }).delete();

// Joins and aggregates
const stats = await db
  .table("orders")
  .join("users", "users.id", "orders.user_id")
  .select("users.country", "SUM(orders.total) as revenue")
  .groupBy("users.country")
  .all();

// Transactions
await db.transaction(async (tx) => {
  await tx.table("accounts").where({ id: 1 }).update({ balance: 100 });
  await tx.table("ledger").insert({ account_id: 1, amount: 100 });
});

// Raw queries for escape hatches, subject to cost estimation
const result = await db.raw<{ count: number }>(
  "SELECT COUNT(*) as count FROM events WHERE created_at > ?",
  [yesterday]
);
```

**Runtime-enforced constraints:**

- **Row limits**: no query returns more than 10,000 rows without explicit opt-in (configurable per deployment)
- **Query timeouts**: 5-second default, configurable up to a ceiling
- **Cost estimation**: raw queries are `EXPLAIN`'d before execution; those exceeding a cost threshold are rejected
- **No schema mutations from worker code**: `CREATE TABLE`, `ALTER`, `DROP` are rejected. Schema changes go through the migrations directory.
- **Required indexes**: the query builder refuses `where({ field })` unless `field` is indexed. The error message tells you to add an index to your migration.
- **Search path enforced per query**: every query has `SET search_path TO deployment_<name>` prepended, so workers can't accidentally reference other deployments' schemas even if they try

**Migrations:**

A directory of timestamped SQL files. The runtime applies them in order at deploy time, tracks applied migrations in a metadata table in the deployment's own schema, and refuses to start the deployment if migrations fail.

```
chirp/
  migrations/
    0001_initial.sql
    0002_add_events_index.sql
    0003_users_table.sql
```

### Redis (for KV and ephemeral state)

Session storage, rate limiting, feature flags, cached fetch results — things where a full database round trip is overkill.

**Architecture:**
- One Redis instance per box, managed by the daemon
- Per-deployment namespacing: worker code calls `kv.get("foo")` and the runtime rewrites it to `deployment_chirp:foo` before it hits Redis
- Deployments cannot address keys outside their prefix because the prefix is injected by the host, not the worker

**Interface:**

```typescript
import { kv } from "@coop/runtime";

await kv.set("session:abc123", JSON.stringify(session), { ex: 3600 });
const session = await kv.get("session:abc123");
const count = await kv.incr("rate_limit:ip:1.2.3.4");
await kv.del("session:abc123");
```

That's it for v0. No pub/sub, no streams, no sorted sets — those come later if real use demands them.

### Object store

Files on disk in v0, wrapped in an S3-ish host function interface.

**Architecture:**
- One directory per deployment under the runtime's data dir
- Host function enforces the path prefix — workers pass keys, the runtime translates to paths
- Interface is deliberately close to S3 so the v1 migration to actual S3/B2/R2 is a config change

**Interface:**

```typescript
import { storage } from "@coop/runtime";

await storage.put("uploads/avatar.jpg", imageBytes, { contentType: "image/jpeg" });
const bytes = await storage.get("uploads/avatar.jpg");
await storage.delete("uploads/old.jpg");

const files = await storage.list({ prefix: "uploads/" });
```

No multipart uploads, no lifecycle policies, no presigned URLs in v0. Just bytes in, bytes out, by key.

### Queue

Backed by Postgres using `SELECT ... FOR UPDATE SKIP LOCKED`, which is a well-known reliable queue pattern that scales to thousands of jobs per second.

**Why Postgres-backed:**
- One less service to run, back up, and understand
- Transactional enqueue (insert a job as part of a transaction that touches other tables — guaranteed either both happen or neither)
- Retries, delays, dead-letter handling all fit naturally into SQL

**Interface:**

```typescript
import { queue } from "@coop/runtime";

// Enqueue
await queue.send("email", {
  to: "user@example.com",
  subject: "Welcome",
  body: "Thanks for signing up",
});

// Delayed enqueue
await queue.send("email", payload, { delay: 60_000 });

// Inside a queue worker file
export default async function process(msg: QueueMessage<EmailPayload>) {
  // ...
  // throw to trigger retry with exponential backoff
  // complete normally to ack
}
```

**Queue config:**
- Concurrency per queue (how many messages can process simultaneously)
- Max retries
- Visibility timeout
- Dead-letter queue name

### Secrets

An encrypted file on disk, decrypted by the daemon on startup using a key from the environment or system keychain. Exposed to workers as a read-only host function.

```typescript
import { secrets } from "@coop/runtime";

const postmarkToken = await secrets.get("POSTMARK_TOKEN");
```

Workers can't list secrets, can't enumerate keys, can only `get` by known name. The list of secrets a deployment can access is in the config and enforced at the host function level.

### Outbound HTTP with retries

`fetch` as a host function, but with built-in retry policies, timeouts, and a connection pool. Every tiny API client in the Skelpo portfolio currently reinvents this.

```typescript
import { fetch } from "@coop/runtime";

const response = await fetch("https://api.stripe.com/v1/charges", {
  method: "POST",
  headers: { "Authorization": `Bearer ${token}` },
  body: JSON.stringify(payload),
  retries: 3,
  timeout: 10_000,
});
```

Allowlisted domains are enforced at the runtime level from the deployment's config.

---

## The full host function surface

This is the complete list of what workers can see. Every capability in Coop is one of these; nothing else exists from the worker's perspective.

```typescript
// @coop/runtime

// Logging
export const log: {
  debug(msg: string, fields?: Record<string, unknown>): void;
  info(msg: string, fields?: Record<string, unknown>): void;
  warn(msg: string, fields?: Record<string, unknown>): void;
  error(msg: string, fields?: Record<string, unknown>): void;
};

// Time
export function now(): number;

// Crypto-quality random
export function random(length: number): Uint8Array;

// Database (typed query builder)
export const db: QueryBuilder;

// Key-value store
export const kv: {
  get(key: string): Promise<string | null>;
  set(key: string, value: string, opts?: { ex?: number }): Promise<void>;
  del(key: string): Promise<number>;
  incr(key: string): Promise<number>;
};

// Object storage
export const storage: {
  get(key: string): Promise<Uint8Array | null>;
  put(key: string, data: Uint8Array, opts?: { contentType?: string }): Promise<void>;
  delete(key: string): Promise<void>;
  list(opts?: { prefix?: string; limit?: number }): Promise<string[]>;
};

// Queue
export const queue: {
  send<T>(queueName: string, payload: T, opts?: { delay?: number }): Promise<void>;
};

// Secrets
export const secrets: {
  get(name: string): Promise<string>;
};

// Outbound HTTP
export function fetch(url: string, init?: FetchInit): Promise<FetchResponse>;

// Request/Response types for HTTP handlers
export interface Request { ... }
export interface Response { ... }
```

**That's the complete worker API surface.** Every future capability is either an extension of one of these or a new host function added to this list. The surface is intentionally small so it can be audited in an afternoon.

---

## Resource isolation and limits

Configurable per deployment, with sensible defaults:

| Limit | Default | Enforcement |
|---|---|---|
| Memory per invocation | 16 MB | Linear memory committed size; allocator returns null on exhaustion |
| CPU per invocation | 100M instructions (~30ms) | Compiler-inserted counter at loop backedges |
| Wall clock per invocation | 30 seconds | Tokio task timeout |
| Stack depth | 256 frames | Compiler-inserted counter at function entry |
| DB connections per deployment | 16 | Runtime connection pool size |
| Max query rows | 10,000 | Query builder + runtime enforcement |
| Max query duration | 5 seconds | Postgres statement timeout per query |
| Concurrent invocations per deployment | 1000 | Tokio semaphore |
| Total memory per deployment | 512 MB | Process-level limit via cgroups |
| Total CPU per deployment | 2 cores | Process-level limit via cgroups |

Banned outright: fork, threads (as a worker-level concept), file descriptors (direct), sockets (direct), subprocess execution.

---

## Directory structure

A deployment is a directory:

```
chirp/
  coop.toml                   # deployment config
  handlers/
    ingest.ts                   # POST /ingest
    query.ts                    # GET /query
    dashboard.ts                # GET /dashboard
  crons/
    daily-aggregate.ts          # runs at 02:00
    cleanup.ts                  # runs every hour
  queues/
    process-event.ts            # processes "events" queue
  migrations/
    0001_initial.sql
    0002_add_indexes.sql
  static/
    index.html
    style.css
```

The runtime's data directory on the box:

```
/var/lib/coop/
  deployments/
    chirp/                      # source, pushed via rsync
    gscmaster/
    fascinating-news/
  compiled/
    chirp.so                    # Perry output, regenerated on deploy
    gscmaster.so
    fascinating-news.so
  storage/                      # object store files
    chirp/
    gscmaster/
  secrets/
    chirp.enc
    gscmaster.enc
  logs/
    coop.sqlite               # structured log store
  runtime.toml                  # daemon config (Postgres URL, Redis URL, etc.)
```

---

## Configuration

Per-deployment config in `coop.toml`:

```toml
name = "chirp"
version = "0.1.4"

[database]
migrations = "./migrations"
max_connections = 16
max_query_rows = 10000
max_query_duration_ms = 5000

[kv]
enabled = true

[storage]
enabled = true

[[handlers]]
file = "handlers/ingest.ts"
path = "/ingest"
method = "POST"

[[handlers]]
file = "handlers/query.ts"
path = "/query"
method = "GET"

[[crons]]
file = "crons/daily-aggregate.ts"
schedule = "0 2 * * *"

[[crons]]
file = "crons/cleanup.ts"
schedule = "0 * * * *"

[[queues]]
file = "queues/process-event.ts"
name = "events"
concurrency = 4
max_retries = 5

[[static]]
directory = "./static"
path = "/"

[capabilities]
fetch = { allowlist = ["api.stripe.com", "api.openai.com"] }
secrets = ["POSTMARK_TOKEN", "STRIPE_WEBHOOK_SECRET"]

[limits]
max_memory_mb_per_invocation = 16
max_instructions_per_invocation = 100_000_000
max_wall_clock_ms = 30000
max_concurrent_invocations = 1000

[limits.process]
max_memory_mb = 512
max_cpu_cores = 2
```

Runtime config in `/var/lib/coop/runtime.toml`:

```toml
[http]
listen = "0.0.0.0:80"
tls_listen = "0.0.0.0:443"
tls_cert = "/etc/letsencrypt/live/coop.dev/fullchain.pem"
tls_key = "/etc/letsencrypt/live/coop.dev/privkey.pem"

[postgres]
url = "postgres://coop:xxx@localhost/coop"
max_connections = 200  # shared across all deployments

[redis]
url = "redis://localhost:6379"

[admin]
path = "/_coop/admin"
password_hash = "..."

[logs]
retention_days = 30

[deployments]
dir = "/var/lib/coop/deployments"
auto_reload = true
```

---

## Observability

### Structured logs

Every invocation produces a log record with timestamp, deployment, worker, trace ID, duration, status, and any fields the worker added via `log.info()`. Stored in a SQLite database under `/var/lib/coop/logs/` (yes, SQLite here — logs are append-only time-series and SQLite handles them beautifully, and we explicitly don't want log storage hitting the same Postgres as application data).

### Request tracing

Every incoming request gets a trace ID that flows through `fetch` calls (injected as a header), database queries, and queue operations. The admin UI shows a tree view of what a request did.

### Metrics

Built-in metrics exposed at `/_coop/metrics` (Prometheus format) and rendered on the admin dashboard:

- Request rate, error rate, latency percentiles (per deployment, per handler)
- Queue depth, queue processing rate, failure rate (per queue)
- Database query rate, slow query log, connection pool utilization (per deployment)
- Memory usage, CPU usage (per deployment process)
- Trap rate (bounds violations, budget exhaustions, panics — per deployment, as a security signal)

### Admin UI

Server-rendered HTML at `/_coop/admin`, protected by a single password. Pages:

- **Dashboard**: list of deployments, status, key metrics
- **Deployment detail**: logs, recent requests, queue state, cron history
- **Request trace**: click a request in the logs, see the full tree
- **Queue management**: retry failed jobs, inspect DLQ, pause/resume queues
- **SQL console**: read-only queries against a deployment's database (helpful at 2am)
- **Deploy history**: what versions have been deployed when

No React, no build step, no JS framework. Server-rendered HTML with a little htmx sprinkled in for interactivity. Ugly is fine; present is the win.

---

## Deployment workflow

### Local development

```bash
# Run a deployment directly, watch mode
coop dev ./chirp

# This spins up an embedded Postgres (via a crate like pg-embed), an embedded
# Redis (via a crate like mini-redis or an actual redis binary), and runs the
# deployment against them. Hot reloads on file change.
```

### Production deployment

```bash
# Push to the box
rsync -av ./chirp/ root@box:/var/lib/coop/deployments/chirp/

# The daemon notices the change (inotify), recompiles, spawns a new
# coop-worker process for chirp, drains the old one, kills it.
# Total downtime: ~1 second.

# Or, if you want to trigger manually:
ssh root@box "coop reload chirp"
```

### Rollback

Previous versions are kept in `/var/lib/coop/compiled/` for N generations (configurable). Rollback is:

```bash
coop rollback chirp
```

Which swaps back to the previous `.so` and restarts the worker process.

---

## The "hosted" pivot

The whole architecture is designed so that turning the internal tool into a hosted product is a configuration change, not a rewrite. Here's what actually has to happen:

1. **Add user accounts to the daemon.** A users table in the shared Postgres, basic auth. ~2 hours.
2. **Tag deployments with an owner.** A column on the deployments metadata. ~30 minutes.
3. **One `coop-worker` process per user (or per group of related users).** The existing per-deployment process model already handles this — just group deployments by user and the daemon spawns workers per user-group rather than per-deployment-within-a-single-user. ~2 hours of daemon logic.
4. **Billing (optional).** Track requests/CPU/storage per user, export to Stripe metered billing. This is where most of the hosted-product work actually lives, and it's comfortably deferrable.
5. **TLS with SNI routing.** A subdomain per user (`username.coop.dev`), ACME for certs. ~a day.

The total architectural work to go from "runs Ralph's portfolio" to "hosted product" is roughly 1-2 days of focused work, because the isolation model (OS process per trust boundary), the resource accounting (cgroups per process), and the per-deployment data isolation (Postgres schemas, Redis prefixes, storage subdirectories) are all already in place.

**What doesn't need to change:**
- The compiler isolation model (already defends against accidental bugs)
- The database architecture (schemas per deployment already exist)
- The storage architecture (per-deployment directories already exist)
- The observability stack (already per-deployment)
- The worker code API (never needs to change regardless of hosting model)

**What does need to change if hosted:**
- Trust boundary moves from "one tenant (Ralph), compiler isolation" to "multiple tenants, OS isolation between tenants, compiler isolation within a tenant"
- Resource limits enforced at the tenant level, not just the deployment level
- Public-facing TLS and subdomain routing
- Billing integration

This is the 2-hour estimate. The non-trivial parts (billing, subdomain TLS) are productization work, not architecture work.

---

## Scaling model

Deliberately limited. Three steps, no more:

### Step 1: vertical scaling

Run on a bigger box. A 16-core, 64GB Hetzner dedicated machine at ~€50/month can run hundreds of Coop deployments comfortably because per-deployment idle overhead is single-digit MB. For the entire Skelpo portfolio, this is the whole answer.

### Step 2: split the database

When Postgres becomes the bottleneck (not the runtime), move it to its own box. The daemon points at a remote Postgres URL; workers don't notice. Same for Redis. This is a config change on the daemon side.

### Step 3: split deployments across boxes

When a single box can't host all deployments, run the daemon on multiple boxes with a shared Postgres/Redis. Each deployment lives on one box (not replicated). The "control plane" is a thin layer on top of the daemon that tells each box which deployments belong to it. Workers still don't notice.

### What's explicitly not supported

- **Horizontal scaling of a single deployment across multiple boxes.** If one deployment needs more than one box, it's outgrown Coop. Go run it on real infrastructure.
- **Multi-region.** One box is in one region. If you need multi-region, run multiple independent Coop instances.
- **Automatic failover.** If the box dies, deployments are down until the box comes back. Backups handle data durability; HA is a future problem.

The honest framing: **this is a vertical-scaling single-box runtime.** That's a feature. The entire class of distributed-systems problems that waste 80% of cloud engineering time simply doesn't apply because there's only one box. You can reason about the whole system because the whole system fits in your head.

---

## v0 scope and timeline

Five weeks of focused work to ship a runnable v0 that can host one real Skelpo deployment (suggest: Chirp, because it's simple and has clear success criteria).

### Week 1: Perry worker runtime core

- Perry `--target worker` mode: TypeScript subset validator, compile error messages
- Perry LLVM backend: bounds-checked memory layout, base-register convention, instruction budget instrumentation, indirect call table
- Worker-mode reimplementation of Perry's allocator and core collections
- Minimal host function set: `log` only
- Proof-of-concept: a compiled Perry TypeScript function runs in isolation with a fresh linear memory

**Exit criterion**: `echo 'export default () => log.info("hi")' | perry worker-compile -o hello.so && coop run hello.so` works.

### Week 2: HTTP server and handler primitive

- Rust daemon: Axum HTTP server, deployment loading, `.so` hot reload via `dlopen`
- Handler dispatch: URL routing, request/response marshaling into worker linear memory
- Host function: `fetch` (synchronous-looking, async under the hood via Tokio)
- Signal handler for SIGSEGV guard traps, using `sigaltstack` and `siglongjmp`

**Exit criterion**: write a TypeScript handler that calls an external API, curl it, get a response back. Measure cold start — should be sub-millisecond.

### Week 3: Postgres integration

- Typed query builder compiled into `@coop/runtime`
- Host functions for database operations
- Per-deployment schema provisioning at deploy time
- Migration runner (applies `migrations/*.sql` in order, tracks in metadata table)
- Query validation: row limits, timeouts, `EXPLAIN`-based cost estimation for raw queries
- Connection pool per deployment

**Exit criterion**: write a handler that accepts a POST, inserts into Postgres via the query builder, responds with JSON. Verify that two deployments' schemas are isolated.

### Week 4: Crons, queues, KV, secrets, static files

- Cron scheduler (Tokio task inside the worker process, parses cron expressions, dispatches to cron entry points)
- Postgres-backed queue using `FOR UPDATE SKIP LOCKED`, with retries and DLQ
- Redis KV host functions with per-deployment key prefixing
- Encrypted secrets file, decryption at daemon startup, host function exposure
- Static file serving via Axum's built-in file service

**Exit criterion**: all four primitives work in one deployment. Port Chirp's ingestion onto it as a test.

### Week 5: Observability and polish

- Structured logging to SQLite log store
- Request tracing with IDs flowing through host functions
- Metrics collection and Prometheus endpoint
- Admin UI (server-rendered HTML, htmx for interactivity)
- `coop` CLI: `dev`, `deploy`, `rollback`, `logs`, `reload`, `status`
- Documentation for writing the first deployment

**Exit criterion**: Chirp is running on Coop in production on a Hetzner box, you can see its metrics in the admin UI, and you've turned off whatever was hosting it before.

### Deferred to v0.1+

- Real async/await in worker code (requires Perry async state machines)
- Object store host functions (probably easy but deferred for focus)
- Multiple deployments on one daemon (architecturally supported but not exercised in v0)
- Hot reload without restart (restart-based reload is fine for v0)
- Dev-mode embedded Postgres/Redis
- Rollback command
- Backup tooling
- TLS termination (run behind nginx or Caddy in v0)

### Deferred to v1+

- The hosted pivot (users, TLS routing, billing)
- Read replicas
- Multi-box deployment distribution
- WASM as an alternate input format
- Node modules allowlist

---

## Critical de-risking experiments

Before committing to the full build, prove two things in 1-2 days each.

### Day 1: LLVM bounds-checked codegen

Write a trivial Perry TypeScript function that allocates an array and loops over it. Compile with whatever LLVM configuration should produce base-relative memory accesses. Disassemble. Verify that loads and stores go through `r15 + offset`, not absolute addresses.

- **If yes**: the codegen strategy works, the rest is implementation work
- **If no**: figure out what Perry's LLVM backend needs to change before writing anything else

### Day 2: Signal handler trap recovery

Minimal Rust program: `mmap` a guard region, install a SIGSEGV handler using `sigaltstack` that `siglongjmp`s back to a trampoline, deliberately fault into the guard. Verify the process survives, the longjmp works, and you can fault multiple times from different "invocations."

- **If yes**: trap handling works
- **If no**: redesign the trap mechanism (possibly move to explicit bounds checks, which are slower but simpler)

If both pass, the rest is implementation work and the timeline is defensible. If either fails, redesign that layer before building on it.

---

## The pitch when you talk about it publicly

Don't call it a Val Town competitor. Don't call it a Heroku replacement. Don't call it a Kubernetes alternative. The framing that actually lands:

> I got tired of running the same boring infrastructure for every small project. So I built a single binary that does all of it, compiles TypeScript to native, and runs my entire portfolio on one €20 box.

Then the numbers: N deployments, M requests/day, X GB memory used, single Hetzner box, zero Docker containers, zero Kubernetes pods. Screenshot of the admin dashboard showing all Skelpo products running. Link to the repo.

This is a build-in-public story that writes itself, plays to the existing LinkedIn/X audience, showcases Perry as the underlying technology without having to explicitly sell Perry, and has a natural "you can run this yourself" call to action.

---

## Honest limitations

What this spec deliberately does not claim:

- **Not production-grade for five-nines systems** in v0 or v0.1. SQLite-for-logs + single-box Postgres = no HA. If you need HA, run real infrastructure.
- **Not a good fit for CPU-bound workloads that need many cores per request.** Workers are designed to be small and many, not big and few.
- **Not a good fit for long-running connections (WebSockets, SSE streams of unlimited duration).** The architecture assumes request-response and cron-style work.
- **Not multi-tenant-hostile-code safe** in v0. Compiler isolation defends against accidents, not active adversaries measuring cache timings.
- **Not multi-region.** One box, one region.
- **Not a platform you should bet your startup on.** It's infrastructure for Ralph's portfolio and whoever else finds it useful. No SLA, no support contract, MIT license, good luck.

What it is: **a tool to replace a dozen Docker containers and seven droplets with one binary, for the kind of small projects that make up 95% of real software.** If that's your problem, it's the right tool. If you need something else, go use something else.

---

*Document version: v0 draft*
*Next step: de-risking experiments (Day 1 + Day 2), then begin Week 1*
