// Checkpoint 1 smoke test — minimal Perch deployment plugin.
//
// Registers a single "route" tool that decodes the DeploymentRequest JSON
// it receives, echoes the method + path + body back as the response, and
// returns the serialized DeploymentResponse. This matches the interim
// wire protocol in perch-host-abi (one "route" tool per deployment,
// JSON-over-string).
//
// Used by crates/perch-worker/tests/ to verify the full dispatch path:
//   perch-worker process
//     → dlopens echo.dylib
//     → plugin_activate registers the "route" tool
//     → accepts a Unix socket connection from the test client
//     → receives a Dispatch request with a DeploymentRequest payload
//     → invokes the "route" tool
//     → returns the DeploymentResponse back over the socket

import { PluginApi } from "perry/plugin";

export function activate(api: PluginApi): void {
  api.setMetadata("echo", "0.0.1", "Checkpoint 1 echo plugin");

  api.registerTool("route", "Echo HTTP router", (argsJson: any) => {
    // argsJson arrives as a string (NaN-boxed STRING_TAG) because
    // perch-worker called perry_plugin_invoke_tool with a string arg.
    // We JSON.parse it into a DeploymentRequest, build a response, and
    // JSON.stringify the result back out — the worker reads the return
    // value as a string and deserializes it into DeploymentResponse.
    const req = JSON.parse(argsJson);

    const bodyText =
      "echo from perry plugin\n" +
      "method: " + req.method + "\n" +
      "path: " + req.path + "\n" +
      "host: " + req.host;

    // base64-encode the body bytes. Perry doesn't ship btoa directly for
    // worker code, so we implement it inline with Buffer. (@perch/runtime
    // will wrap this pattern once we have the runtime library.)
    const bodyB64 = Buffer.from(bodyText, "utf-8").toString("base64");

    const response = {
      status: 200,
      headers: { "content-type": "text/plain" },
      body_base64: bodyB64,
    };

    return JSON.stringify(response);
  });
}

export function deactivate(): void {
  // nothing
}
