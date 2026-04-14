// Test 4: CPU-heavy — recursive fibonacci on every request.
// Measures native code throughput from Perry's LLVM backend.
function fib(n: number): number {
  if (n < 2) return n;
  return fib(n - 1) + fib(n - 2);
}

export function handle(reqJson: string): string {
  const result = fib(28); // ~317k calls, takes a few ms
  const body = "fib(28)=" + result;
  const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "text/plain" },
    body_base64: bodyB64,
  });
}
