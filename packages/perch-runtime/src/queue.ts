// @perch/runtime — Queue enqueue.
//
// The enqueue side of the Postgres-backed queue (SELECT ... FOR UPDATE
// SKIP LOCKED). The polling side lives in perch-worker (Rust).
//
// Usage:
//   import { queue } from "@perch/runtime";
//   await queue.send("email", { to: "user@example.com", subject: "Welcome" });
//   await queue.send("email", payload, { delay: 60000 });

import { db } from "./db";

export const queue = {
  async send(queueName: string, payload: any, opts?: { delay?: number }): Promise<void> {
    const delayMs = opts?.delay || 0;
    const visibleAt = Date.now() + delayMs;

    await db.table("_perch_queue").insert({
      queue_name: queueName,
      payload: JSON.stringify(payload),
      visible_at: visibleAt,
      attempts: 0,
      created_at: Date.now(),
    });
  },
};
