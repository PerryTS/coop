// @perch/runtime — Key-value store (Redis).
//
// Wraps Perry's ioredis stdlib. perch-worker sets PERCH_REDIS_URL and
// PERCH_REDIS_PREFIX env vars; the prefix is prepended to every key so
// deployments can't collide.
//
// Usage:
//   import { kv } from "@perch/runtime";
//   await kv.set("session:abc", JSON.stringify(data), { ex: 3600 });
//   const val = await kv.get("session:abc");
//
// ──────────────────────────────────────────────────────────────────────
// What Perry's ioredis actually supports, and what that costs us
// ──────────────────────────────────────────────────────────────────────
//
// This module is compiled by Perry, not Node, and Perry's ioredis binding is
// a fixed compile-time dispatch table
// (.perry-main/crates/perry-codegen/src/lower_call/native_table/databases.rs).
// A method with no row in that table does not lower to a call at all — it
// falls through to the generic dispatcher and evaluates to `undefined`, with
// no compile error and no runtime error. Every method name below is one that
// has a row. Do not add others without checking that table first.
//
// Reachable: set(k,v) get(k) del(k) exists(k) incr(k) decr(k)
//            expire(k,seconds) connect() quit() disconnect()
// NOT reachable (silently undefined): setex, ttl, mget, mset, keys, scan,
//            incrby, type, setnx, persist, getset, pipeline/multi, and every
//            list/set/hash/pubsub command.
//
// Two consequences are visible in the API:
//
//  1. `set(k, v, { ex })` is NOT atomic. Node's ioredis would issue
//     `SET k v EX n`, but Perry's `set` row declares exactly two string
//     arguments, so a third and fourth argument are dropped rather than
//     forwarded — the TTL would be silently lost. `js_ioredis_setex` exists in
//     the runtime but has no dispatch row, so `setex` is equally unreachable.
//     The only reachable path is SET followed by EXPIRE, which is two round
//     trips and leaves a window in which the key exists without its TTL. A
//     crash inside that window leaves a key that never expires. We check the
//     EXPIRE reply and throw if it did not apply, so the failure is loud.
//
//  2. `new Redis(...)` ignores its argument. Perry lowers the constructor to
//     `js_ioredis_new(0)` and the runtime builds its connection URL from
//     REDIS_HOST / REDIS_PORT / REDIS_PASSWORD / REDIS_TLS — with TLS
//     defaulting to ON. Passing PERCH_REDIS_URL here would do nothing.
//     perch-worker translates PERCH_REDIS_URL into those four variables before
//     loading this library (crates/perch-worker/src/deployment_env.rs); this
//     module treats PERCH_REDIS_URL purely as the "is kv configured?" flag.

import Redis from "ioredis";

// Read at module-init time, which Perry runs inside perry_module_init() while
// perch-worker's DeploymentHost::load is still on the stack — after the worker
// has exported the environment.
const PREFIX = process.env.PERCH_REDIS_PREFIX || "";
const REDIS_URL = process.env.PERCH_REDIS_URL || "";

// Constructed unconditionally at module scope. This is deliberate and not
// merely convenient: Perry resolves `client.get(...)` to a native ioredis call
// through the *static* type of the receiver, so a lazily-assigned `let
// client: any` would not be recognised as an ioredis handle and the calls
// would dispatch to nothing. Construction is free — `js_ioredis_new` only
// registers a handle and records a URL; no socket is opened until the first
// command.
const client = new Redis();

function prefixed(key: string): string {
  return PREFIX + key;
}

/// Fail before touching Redis when the host did not configure this
/// deployment. Without a prefix every deployment on the box would share one
/// keyspace, so an unset prefix is a data-isolation break, not a default.
function checkConfigured(): void {
  if (PREFIX === "") {
    throw new Error(
      "kv is not available: PERCH_REDIS_PREFIX is not set. Without a " +
      "host-assigned key prefix this deployment would share a keyspace with " +
      "every other deployment on the box. kv requires a dedicated worker " +
      "process — set isolation.class = \"dedicated\" in perch.toml."
    );
  }
  if (REDIS_URL === "") {
    throw new Error(
      "kv is not configured: PERCH_REDIS_URL is not set. Configure Redis in " +
      "runtime.toml [redis] url so perch-worker can export it to this " +
      "deployment."
    );
  }
}

function checkKey(key: string): void {
  if (typeof key !== "string" || key.length === 0) {
    throw new TypeError("kv key must be a non-empty string");
  }
}

/// Reject a TTL Redis cannot express before spending a SET on it. EXPIRE takes
/// whole seconds; a fractional or zero value would be truncated to something
/// the caller did not ask for.
function expirySeconds(opts?: { ex?: number }): number {
  const ex = opts?.ex;
  if (ex === undefined || ex === null) return 0;
  if (typeof ex !== "number" || !Number.isFinite(ex)) {
    throw new TypeError("kv set { ex } must be a finite number of seconds");
  }
  if (!Number.isInteger(ex) || ex < 1) {
    throw new RangeError(
      "kv set { ex } must be a whole number of seconds >= 1; Redis EXPIRE " +
      "has one-second resolution and would truncate " + ex
    );
  }
  return ex;
}

export const kv = {
  async get(key: string): Promise<string | null> {
    checkConfigured();
    checkKey(key);
    const value = await client.get(prefixed(key));
    // The binding resolves a missing key to null; normalise undefined so
    // callers only ever have one absent case to handle.
    return value === undefined ? null : value;
  },

  async set(key: string, value: string, opts?: { ex?: number }): Promise<void> {
    checkConfigured();
    checkKey(key);
    if (typeof value !== "string") {
      throw new TypeError("kv value must be a string; serialize it first");
    }
    const ex = expirySeconds(opts);
    const full = prefixed(key);
    await client.set(full, value);
    if (ex > 0) {
      // See the header: SET+EXPIRE rather than SET ... EX, because Perry's
      // `set` row takes exactly two arguments and would drop the rest.
      const applied = await client.expire(full, ex);
      if (applied !== 1) {
        throw new Error(
          "kv set stored '" + key + "' but its " + ex + "s TTL was not " +
          "applied (EXPIRE returned " + applied + "); the key may never expire"
        );
      }
    }
  },

  async del(key: string): Promise<number> {
    checkConfigured();
    checkKey(key);
    return await client.del(prefixed(key));
  },

  async incr(key: string): Promise<number> {
    checkConfigured();
    checkKey(key);
    return await client.incr(prefixed(key));
  },
};
