// Test 8: Postgres SELECT via Perry's pg stdlib.
// For now this is a placeholder that demonstrates the import shape.
// Real DB access lands when @perch/runtime db.ts wires Perry's pg stdlib
// (Phase B in the plan). For the stress test we just respond with a
// fake row count so we can compare RAM with vs without DB code paths.
export function handle(reqJson: string): string {
  // Simulated query result — real INSERT/SELECT lands with Phase B.
  const fakeRow = { id: 1, name: "test", value: 42 };
  const body = JSON.stringify({ source: "postgres-stub", row: fakeRow });
  const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "application/json" },
    body_base64: bodyB64,
  });
}
