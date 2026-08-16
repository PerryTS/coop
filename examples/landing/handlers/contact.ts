// Strict COOP Buffer handler for the landing contact form. HTTP metadata and
// body bytes stay binary across the host ABI; no JSON/Base64 fallback exists.

type Cursor = { offset: number };

function readBytes(frame: Buffer, cursor: Cursor): Buffer {
  if (cursor.offset + 4 > frame.length) throw new Error("truncated COOP length");
  const length = frame.readUInt32BE(cursor.offset);
  cursor.offset += 4;
  if (cursor.offset + length > frame.length) throw new Error("truncated COOP field");
  const value = frame.subarray(cursor.offset, cursor.offset + length);
  cursor.offset += length;
  return value;
}

function response(status: number, headerName: string, headerValue: string, body: Buffer): Buffer {
  const name = Buffer.from(headerName);
  const value = Buffer.from(headerValue);
  const output = Buffer.alloc(5 + 2 + 4 + 4 + name.length + 4 + value.length + 4 + body.length);
  output[0] = 0x50; output[1] = 0x43; output[2] = 0x48; output[3] = 0x32; output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(status, offset); offset += 2;
  output.writeUInt32BE(1, offset); offset += 4;
  output.writeUInt32BE(name.length, offset); offset += 4;
  name.copy(output, offset); offset += name.length;
  output.writeUInt32BE(value.length, offset); offset += 4;
  value.copy(output, offset); offset += value.length;
  output.writeUInt32BE(body.length, offset); offset += 4;
  body.copy(output, offset);
  return output;
}

export function handle(frame: Buffer): Buffer {
  if (frame.length < 5 || frame[0] !== 0x50 || frame[1] !== 0x43 ||
      frame[2] !== 0x48 || frame[3] !== 0x32 || frame[4] !== 1) {
    throw new Error("invalid COOP HTTP request");
  }
  const cursor: Cursor = { offset: 5 };
  const method = readBytes(frame, cursor).toString("utf-8");
  const path = readBytes(frame, cursor).toString("utf-8");
  // query, remote address, scheme, and host
  for (let i = 0; i < 4; i++) readBytes(frame, cursor);
  if (cursor.offset + 4 > frame.length) throw new Error("truncated COOP headers");
  const headerCount = frame.readUInt32BE(cursor.offset); cursor.offset += 4;
  for (let i = 0; i < headerCount; i++) {
    readBytes(frame, cursor);
    readBytes(frame, cursor);
  }
  const body = readBytes(frame, cursor);
  if (cursor.offset !== frame.length) throw new Error("trailing COOP bytes");
  const bodyText = body.toString("utf-8");

  // Route: POST /contact
  if (method === "POST" && path === "/contact") {
    // Parse URL-encoded form data.
    const form: Record<string, string> = {};
    const pairs = bodyText.split("&");
    for (let i = 0; i < pairs.length; i++) {
      const pair = pairs[i];
      const eqIdx = pair.indexOf("=");
      if (eqIdx >= 0) {
        const key = decodeURIComponent(pair.substring(0, eqIdx));
        const value = decodeURIComponent(pair.substring(eqIdx + 1).replace(/\+/g, " "));
        form[key] = value;
      }
    }

    const email = form["email"] || "(no email)";
    const message = form["message"] || "(no message)";

    // Log the submission (flows to daemon via stdout).
    console.log(JSON.stringify({
      ts: Date.now(),
      level: "info",
      msg: "contact form submission",
      email: email,
      message_preview: message.substring(0, 100),
    }));

    // Redirect to thank-you page.
    return response(303, "location", "/thanks.html", Buffer.alloc(0));
  }

  // Default: 404 for unmatched routes.
  return response(404, "content-type", "text/plain", Buffer.from("Not Found"));
}
