// Test 1: minimal handler. Returns a fixed string. No work done.
// Baseline for raw HTTP overhead and per-deployment RAM cost.
export function handle(reqJson: string): string {
  const body = "ok";
  const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "text/plain" },
    body_base64: bodyB64,
  });
}
