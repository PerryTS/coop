// Test 5: large JSON response (~50 KB).
export function handle(reqJson: string): string {
  const items: any[] = [];
  for (let i = 0; i < 500; i++) {
    items.push({
      id: i,
      name: "Item " + i,
      description: "This is a description for item number " + i + " in the list.",
      timestamp: Date.now() + i,
      tags: ["alpha", "beta", "gamma"],
    });
  }
  const body = JSON.stringify({ count: items.length, items: items });
  const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "application/json" },
    body_base64: bodyB64,
  });
}
