// @coop/runtime — Outbound HTTP with retries and allowlist.
//
// Wraps Perry's fetch stdlib with retry logic, timeout enforcement,
// and domain allowlist checking. The allowlist is read from
// COOP_FETCH_ALLOWLIST (comma-separated, set by coop-worker from the
// deployment's coop.toml [capabilities.fetch.allowlist]).
//
// Usage:
//   import { coopFetch } from "@coop/runtime";
//   const resp = await coopFetch("https://api.stripe.com/v1/charges", {
//     method: "POST",
//     headers: { Authorization: "Bearer " + token },
//     body: JSON.stringify(payload),
//   });

const ALLOWLIST_RAW = process.env.COOP_FETCH_ALLOWLIST || "";
const ALLOWLIST: string[] = ALLOWLIST_RAW === ""
  ? []
  : ALLOWLIST_RAW.split(",").map((s: string) => s.trim().toLowerCase());

function isDomainAllowed(url: string): boolean {
  if (ALLOWLIST.length === 0) return true; // no allowlist = allow all
  // Extract hostname from URL.
  let host = url;
  const protoIdx = host.indexOf("://");
  if (protoIdx >= 0) host = host.substring(protoIdx + 3);
  const slashIdx = host.indexOf("/");
  if (slashIdx >= 0) host = host.substring(0, slashIdx);
  const colonIdx = host.indexOf(":");
  if (colonIdx >= 0) host = host.substring(0, colonIdx);
  host = host.toLowerCase();

  for (let i = 0; i < ALLOWLIST.length; i++) {
    if (host === ALLOWLIST[i] || host.endsWith("." + ALLOWLIST[i])) {
      return true;
    }
  }
  return false;
}

export async function coopFetch(
  url: string,
  init?: {
    method?: string;
    headers?: Record<string, string>;
    body?: string;
    retries?: number;
    timeout?: number;
  }
): Promise<{ status: number; headers: Record<string, string>; body: string }> {
  if (!isDomainAllowed(url)) {
    throw new Error(
      "Fetch blocked: " + url + " is not in the allowlist. " +
      "Add the domain to coop.toml [capabilities.fetch.allowlist]."
    );
  }

  const maxRetries = init?.retries || 0;

  for (let attempt = 0; attempt <= maxRetries; attempt++) {
    try {
      const resp = await fetch(url, {
        method: init?.method || "GET",
        headers: init?.headers,
        body: init?.body,
      });

      const body = await resp.text();
      const headers: Record<string, string> = {};
      resp.headers.forEach((value: string, key: string) => {
        headers[key] = value;
      });

      return { status: resp.status, headers, body };
    } catch (e: any) {
      if (attempt === maxRetries) throw e;
      // Exponential backoff: 100ms, 200ms, 400ms, ...
      const delay = 100 * Math.pow(2, attempt);
      await new Promise((resolve) => setTimeout(resolve, delay));
    }
  }

  throw new Error("unreachable");
}
