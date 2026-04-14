// Handler for the "hello" deployment. Filename is hello-handler.ts so
// Perry creates symbols named perry_fn_hello_handler_ts__handle.
//
// But since the deployment is named "hello" and the dylib will be
// "hello.dylib", we'll need module_name_from_path to derive the
// right module name. Let's just name this file "hello.ts" instead.
//
// Nope — hello.ts already exists (the Phase A.2 plugin). Let me use a
// different approach and have the e2e test use the correct dylib name.

export function handle(reqJson: string): string {
  const req = JSON.parse(reqJson);

  const body = "echo from perch\n" +
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
