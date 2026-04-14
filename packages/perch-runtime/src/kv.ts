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

const PREFIX = process.env.PERCH_REDIS_PREFIX || "";

function prefixed(key: string): string {
  return PREFIX + key;
}

export const kv = {
  async get(key: string): Promise<string | null> {
    // TODO: wire to Perry's ioredis stdlib
    //   import Redis from "ioredis";
    //   const redis = new Redis(process.env.PERCH_REDIS_URL);
    //   return redis.get(prefixed(key));
    console.log(JSON.stringify({ _perch_kv: "get", key: prefixed(key) }));
    return null;
  },

  async set(key: string, value: string, opts?: { ex?: number }): Promise<void> {
    console.log(JSON.stringify({ _perch_kv: "set", key: prefixed(key), ex: opts?.ex }));
  },

  async del(key: string): Promise<number> {
    console.log(JSON.stringify({ _perch_kv: "del", key: prefixed(key) }));
    return 0;
  },

  async incr(key: string): Promise<number> {
    console.log(JSON.stringify({ _perch_kv: "incr", key: prefixed(key) }));
    return 0;
  },
};
