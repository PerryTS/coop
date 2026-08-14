import { createServer } from "node:http";

const appCount = Number.parseInt(process.env.NODE_APP_COUNT ?? "1", 10);
const handlers = Array.from({ length: appCount }, () => (_request, response) => {
  response.writeHead(200, {
    "content-type": "text/plain",
    "content-length": "2",
  });
  response.end("ok");
});

const server = createServer((request, response) => {
  const match = /^bench-(\d+)\.bench/.exec(request.headers.host ?? "");
  const index = match ? Number.parseInt(match[1], 10) : 0;
  const handler = handlers[index];
  if (handler === undefined) {
    response.writeHead(404);
    response.end();
    return;
  }
  handler(request, response);
});

server.listen(0, "127.0.0.1", () => {
  const address = server.address();
  process.stdout.write(`READY ${address.port}\n`);
});
