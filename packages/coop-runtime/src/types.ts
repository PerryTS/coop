// @coop/runtime — Request/Response types and body parsing helpers.
//
// These match the JSON wire protocol in coop-host-abi. coop-worker
// serializes the incoming HTTP request as a DeploymentRequest JSON string
// and passes it to the handler's `handle(reqJson: string): string`
// function. The handler deserializes it into a CoopRequest, does its
// work, and returns a serialized CoopResponse.
//
// This module wraps that JSON-over-string protocol with a clean API so
// deployment authors write:
//
//   import { CoopRequest, CoopResponse, respond } from "@coop/runtime";
//
//   export function handle(reqJson: string): string {
//     const req = new CoopRequest(reqJson);
//     const body = req.text();
//     return respond(200, { "content-type": "text/plain" }, "Hello!");
//   }

// ── Request ──

export class CoopRequest {
  method: string;
  path: string;
  query: string;
  headers: Record<string, string>;
  remoteAddr: string;
  scheme: string;
  host: string;
  private _bodyBase64: string;
  private _bodyBytes: number[] | null;

  constructor(json: string) {
    const raw = JSON.parse(json);
    this.method = raw.method || "GET";
    this.path = raw.path || "/";
    this.query = raw.query || "";
    this.headers = raw.headers || {};
    this.remoteAddr = raw.remote_addr || "";
    this.scheme = raw.scheme || "http";
    this.host = raw.host || "";
    this._bodyBase64 = raw.body_base64 || "";
    this._bodyBytes = null;
  }

  // Decode base64 body to bytes (cached).
  private bodyBytes(): number[] {
    if (this._bodyBytes === null) {
      if (this._bodyBase64 === "") {
        this._bodyBytes = [];
      } else {
        const buf = Buffer.from(this._bodyBase64, "base64");
        this._bodyBytes = [];
        for (let i = 0; i < buf.length; i++) {
          this._bodyBytes.push(buf[i] as number);
        }
      }
    }
    return this._bodyBytes as number[];
  }

  // Body as UTF-8 string.
  text(): string {
    const bytes = this.bodyBytes();
    if (bytes.length === 0) return "";
    return Buffer.from(bytes).toString("utf-8");
  }

  // Body parsed as JSON.
  json(): any {
    return JSON.parse(this.text());
  }

  // Body parsed as URL-encoded form data.
  // Returns a Map-like object: { email: "foo@bar.com", message: "hi" }
  formData(): Record<string, string> {
    const text = this.text();
    const result: Record<string, string> = {};
    if (text === "") return result;

    const pairs = text.split("&");
    for (let i = 0; i < pairs.length; i++) {
      const pair = pairs[i];
      const eqIdx = pair.indexOf("=");
      if (eqIdx === -1) {
        result[decodeURIComponent(pair)] = "";
      } else {
        const key = decodeURIComponent(pair.substring(0, eqIdx));
        const value = decodeURIComponent(pair.substring(eqIdx + 1).replace(/\+/g, " "));
        result[key] = value;
      }
    }
    return result;
  }

  // Get a single header by name (case-insensitive).
  header(name: string): string | undefined {
    return this.headers[name.toLowerCase()];
  }

  // Get the client's IP address (respects trusted proxy headers when
  // the request came through a proxy).
  ip(): string {
    return this.remoteAddr;
  }

  // Get a query parameter by name.
  queryParam(name: string): string | undefined {
    if (this.query === "") return undefined;
    const pairs = this.query.split("&");
    for (let i = 0; i < pairs.length; i++) {
      const pair = pairs[i];
      const eqIdx = pair.indexOf("=");
      if (eqIdx === -1 && decodeURIComponent(pair) === name) return "";
      if (eqIdx >= 0 && decodeURIComponent(pair.substring(0, eqIdx)) === name) {
        return decodeURIComponent(pair.substring(eqIdx + 1).replace(/\+/g, " "));
      }
    }
    return undefined;
  }
}

// ── Response helpers ──

// Build a JSON response string matching the DeploymentResponse wire format.
export function respond(
  status: number,
  headers: Record<string, string>,
  body: string
): string {
  const bodyB64 = Buffer.from(body, "utf-8").toString("base64");
  return JSON.stringify({
    status: status,
    headers: headers,
    body_base64: bodyB64,
  });
}

// Redirect response (303 See Other by default).
export function redirect(url: string, status: number = 303): string {
  return respond(status, { location: url }, "");
}

// JSON response with correct content-type.
export function jsonResponse(status: number, data: any): string {
  const body = JSON.stringify(data);
  return respond(status, { "content-type": "application/json" }, body);
}

// Set cache header for downstream/edge caching.
export function withCacheHeaders(
  responseJson: string,
  maxAgeSecs: number
): string {
  const resp = JSON.parse(responseJson);
  resp.headers = resp.headers || {};
  resp.headers["cache-control"] = "public, max-age=" + maxAgeSecs + ", s-maxage=" + maxAgeSecs;
  return JSON.stringify(resp);
}
