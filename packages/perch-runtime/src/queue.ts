// @perch/runtime — host-owned durable queue enqueue.
//
// Usage:
//   import { queue } from "@perch/runtime";
//   await queue.send("email", { to: "user@example.com", subject: "Welcome" });
//   await queue.send("email", payload, { delay: 60000 });
//   await queue.sendRaw("binary", Buffer.from([0, 255]));

declare function js_perch_queue_enqueue(
  queueName: string,
  payloadJson: string,
  delayMs: number,
): number;

declare function js_perch_queue_enqueue_raw(
  queueName: string,
  payload: Buffer,
  delayMs: number,
): number;

type QueueSendOptions = { delay?: number };

function queueDelay(opts?: QueueSendOptions): number {
  const delayMs = opts?.delay ?? 0;
  if (!Number.isFinite(delayMs) || delayMs < 0) {
    throw new Error("queue delay must be a finite non-negative number");
  }
  return Math.floor(delayMs);
}

function checkQueueName(queueName: string): void {
  if (queueName.length === 0) throw new Error("queue name must not be empty");
}

function checkResult(result: number): void {
  if (result !== 0) {
    throw new Error("Perch durable queue enqueue failed with code " + result);
  }
}

export const queue = {
  async send(queueName: string, payload: any, opts?: QueueSendOptions): Promise<void> {
    checkQueueName(queueName);
    const encoded = JSON.stringify(payload);
    if (encoded === undefined) {
      throw new Error("queue payload must be JSON serializable");
    }
    checkResult(js_perch_queue_enqueue(queueName, encoded, queueDelay(opts)));
  },

  async sendRaw(queueName: string, payload: Buffer, opts?: QueueSendOptions): Promise<void> {
    checkQueueName(queueName);
    if (!Buffer.isBuffer(payload)) {
      throw new TypeError("queue raw payload must be a Buffer");
    }
    checkResult(js_perch_queue_enqueue_raw(queueName, payload, queueDelay(opts)));
  },
};
