// v0.5 shape: just a plain export, no plugin API. Perry compiles this to
// _perry_fn_echo_v5_ts__handle (exported symbol). coop-worker dlopens and
// calls it directly via dlsym.
//
// The function takes a JSON string (the DeploymentRequest), parses it,
// does its work, and returns a JSON string (the DeploymentResponse).

export function handle(reqJson: string): string {
  const req = JSON.parse(reqJson);

  const body = "echo from v5\n" +
    "method: " + req.method + "\n" +
    "path: " + req.path + "\n" +
    "host: " + req.host;

  const response = {
    status: 200,
    headers: { "content-type": "text/plain" },
    body_base64: Buffer.from(body, "utf-8").toString("base64"),
  };

  return JSON.stringify(response);
}
