// Test 7: allocation-heavy — builds large arrays + Maps per request.
// Stresses Perry's GC / arena allocator.
export function handle(reqJson: string): string {
  const map = new Map<string, number>();
  const arr: number[] = [];

  for (let i = 0; i < 10000; i++) {
    arr.push(i * 2);
    map.set("key_" + i, i);
  }

  let sum = 0;
  for (let i = 0; i < arr.length; i++) {
    sum += arr[i];
  }

  const result = "items=" + arr.length + " map=" + map.size + " sum=" + sum;
  const bodyB64 = Buffer.from(result, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "text/plain" },
    body_base64: bodyB64,
  });
}
