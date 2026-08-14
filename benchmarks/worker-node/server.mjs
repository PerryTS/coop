import { createServer } from "node:http";

const port = Number.parseInt(process.env.PORT ?? "4581", 10);

const server = createServer((request, response) => {
  const url = new URL(request.url ?? "/", "http://benchmark.local");
  const requested = Number(url.searchParams.get("iterations") ?? "100");
  const iterations = Number.isFinite(requested)
    ? Math.max(1, Math.min(10_000, Math.trunc(requested)))
    : 100;

  let checksum = 0x811c9dc5;
  for (let index = 0; index < iterations; index += 1) {
    checksum = Math.imul(checksum ^ index, 0x01000193) >>> 0;
  }

  const body = JSON.stringify({ runtime: "node", iterations, checksum });
  response.writeHead(200, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(body),
  });
  response.end(body);
});

server.listen(port, "127.0.0.1");
