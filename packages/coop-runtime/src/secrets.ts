// @coop/runtime — Secrets accessor.
//
// coop-worker decrypts the deployment's secrets file at startup and
// sets COOP_SECRET_<NAME> env vars. This module provides a clean
// accessor that returns the value or throws if the secret isn't
// configured.
//
// Usage:
//   import { secrets } from "@coop/runtime";
//   const token = secrets.get("POSTMARK_TOKEN");

export const secrets = {
  get(name: string): string {
    const envKey = "COOP_SECRET_" + name;
    const value = process.env[envKey];
    if (value === undefined || value === "") {
      throw new Error(
        "Secret '" + name + "' not found. " +
        "Add it to the deployment's secrets file and list it in " +
        "coop.toml [capabilities] secrets = [\"" + name + "\"]."
      );
    }
    return value;
  },
};
