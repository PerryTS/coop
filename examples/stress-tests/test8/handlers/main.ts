// Test 8: real Postgres queries via Perry's pg stdlib + async/await.
//
// Each request:
// 1. Connects to Postgres (or reuses cached connection)
// 2. Inserts a row into `hits` recording the request path
// 3. Counts total rows
// 4. Returns JSON {hits: count, path: req.path}
//
// Demonstrates the full Perch async story: handler awaits database
// operations, perch-worker drives Perry's event loop until the Promise
// resolves, response flows back through the daemon to the client.

import { Client } from "pg";

let client: Client | null = null;

async function getClient(): Promise<Client> {
  if (client === null) {
    client = new Client({
      connectionString: process.env.PERCH_DB_URL || "postgres://postgres@localhost/perch",
    });
    await client.connect();
  }
  return client;
}

export async function handle(reqJson: string): Promise<string> {
  const req = JSON.parse(reqJson);

  try {
    const c = await getClient();

    // Record this hit (raw query, no params — Perry's pg parameterized
    // queries are still TBD; we use literal SQL with the path embedded
    // safely because it comes from req.path which is a controlled value).
    const path = (req.path || "/").replace(/[^a-zA-Z0-9/._-]/g, "");
    await c.query("INSERT INTO hits (path) VALUES ('" + path + "')");

    // Count total hits.
    const result = await c.query("SELECT COUNT(*) AS n FROM hits");
    const count = result.rows[0]?.n || 0;

    const data = {
      hits: count,
      path: req.path,
      method: req.method,
      timestamp: Date.now(),
    };
    const body = JSON.stringify(data);
    const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
    return JSON.stringify({
      status: 200,
      headers: { "content-type": "application/json" },
      body_base64: bodyB64,
    });
  } catch (e: any) {
    const body = JSON.stringify({ error: String(e?.message || e) });
    const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
    return JSON.stringify({
      status: 500,
      headers: { "content-type": "application/json" },
      body_base64: bodyB64,
    });
  }
}
