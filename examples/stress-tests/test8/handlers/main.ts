import { connect, parseConnectionString } from "@perry/postgres/src/index";

export async function handle(reqJson: string): Promise<string> {
  const req = JSON.parse(reqJson);

  try {
    const url = process.env.PERCH_DB_URL;
    if (!url || url === "") {
      throw new Error("PERCH_DB_URL not set");
    }

    const conn = await connect(parseConnectionString(url));

    // Insert a hit row.
    await conn.query("INSERT INTO hits (path) VALUES ('" + req.path + "')");

    // Count total hits.
    const result = await conn.query("SELECT COUNT(*)::int4 AS n FROM hits");
    const count = result.rows.length > 0 ? (result.rows[0] as any).n : 0;

    await conn.close();

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
