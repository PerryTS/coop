// Test 3: JSON in/out — parses request, builds JSON response.
export function handle(reqJson: string): string {
  const req = JSON.parse(reqJson);

  if (req.method === "POST" && req.path === "/echo") {
    return JSON.stringify({
      status: 200,
      headers: { "content-type": "application/json" },
      body_base64: req.body_base64,
    });
  }

  const data = {
    deployment: "test3",
    method: req.method,
    path: req.path,
    host: req.host,
    timestamp: Date.now(),
  };
  const body = JSON.stringify(data);
  const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "application/json" },
    body_base64: bodyB64,
  });
}
