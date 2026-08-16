// Phase A.1 — minimal Perry plugin for the dylib derisk experiment.
//
// Plugin TS imports PluginApi from "perry/plugin" so the codegen knows to
// lower api.registerTool / setMetadata calls to the perry_plugin_register_*
// FFI symbols. With Perry's needs_plugins gate fix landed, the codegen now
// declares those externs even when the source has no perry/ui import — the
// canonical Coop deployment shape.
//
// Each Coop deployment is a Perry plugin compiled with --output-type dylib.
// coop-worker is a Rust process that links perry-runtime, dlopens the
// deployment .dylib, and dispatches HTTP requests by invoking the closures
// the deployment registered via api.registerRoute / registerTool.
//
// For Phase A.2 we register a single tool ("greet") and verify that a Rust
// host can dlopen the dylib, invoke the tool, and get the string back.

import { PluginApi } from "perry/plugin";

export function activate(api: PluginApi): void {
  api.setMetadata("hello", "0.0.1", "Phase A derisk plugin");

  api.registerTool("greet", "Phase A derisk tool", (args: any) => {
    return "hello from perry plugin";
  });
}

export function deactivate(): void {
  // nothing
}
