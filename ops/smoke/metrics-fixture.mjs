import http from "node:http";

const host = "127.0.0.1";
const port = Number.parseInt(process.env.PERCH_SMOKE_METRICS_PORT ?? "19101", 10);
if (!Number.isInteger(port) || port < 1 || port > 65535) {
  throw new Error("PERCH_SMOKE_METRICS_PORT must be a valid TCP port");
}

const metrics = `# TYPE perch_deployments_total gauge
perch_deployments_total 1
# TYPE perch_deployment_process_isolated gauge
perch_deployment_process_isolated{deployment="smoke-app"} 0
# TYPE perch_requests_total counter
perch_requests_total{deployment="smoke-app",method="GET",status="200"} 1
# TYPE perch_request_duration_seconds histogram
perch_request_duration_seconds_bucket{deployment="smoke-app",method="GET",status="200",le="0.005"} 1
perch_request_duration_seconds_bucket{deployment="smoke-app",method="GET",status="200",le="+Inf"} 1
perch_request_duration_seconds_sum{deployment="smoke-app",method="GET",status="200"} 0.001
perch_request_duration_seconds_count{deployment="smoke-app",method="GET",status="200"} 1
`;

const server = http.createServer((request, response) => {
  if (request.url === "/metrics") {
    response.writeHead(200, { "content-type": "text/plain; version=0.0.4" });
    response.end(metrics);
    return;
  }
  if (request.url === "/ready") {
    response.writeHead(200, { "content-type": "text/plain" });
    response.end("ready\n");
    return;
  }
  response.writeHead(404).end();
});

server.listen(port, host);
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
