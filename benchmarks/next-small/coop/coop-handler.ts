import { NextRequest } from "next/server";
import { GET } from "../app/api/benchmark/route";

/**
 * Adapt Coop's compact wire protocol to the same Next.js App Route function
 * the production Node build serves.
 *
 * WHAT CHANGED, AND WHY IT MATTERS FOR EVERY NUMBER THIS FIXTURE PRODUCES.
 *
 * This adapter used to call `GET(nextRequest)`, throw the result away, and
 * emit a hardcoded 200 with a hardcoded body and a hardcoded checksum. It ran
 * the framework work and then fabricated the answer. Any measurement taken
 * against it was a lower bound on Perry's cost by construction, and no
 * assertion about the response could fail, because the response never came
 * from Next.
 *
 * That was not dishonesty in the original, it was a workaround: the Perry pin
 * of the day could not carry a `Response`'s status, headers, or body across
 * this imported-function boundary. #8036 and #8038 fixed exactly that, and the
 * pin now includes them, so the workaround can go.
 *
 * The response is now READ FROM NEXT: status, headers and body all come from
 * the `NextResponse` the route returned. If the transport regresses, this
 * fixture fails loudly rather than quietly reporting a fabricated 200 -- which
 * is the property that makes it worth benchmarking at all.
 *
 * Still a lower bound in one respect: it invokes the userland `GET` export
 * rather than Next's private `AppRouteRouteModule.handle`, so the work-store
 * machinery around the route is not exercised. That is the next step, and it
 * needs the full production build output rather than the source route. Until
 * then this fixture must not be described as full Next hosting.
 */

async function executeNextRoute(
  rawMethod: string,
  path: string,
  query: string,
  scheme: string,
  host: string,
  headers: Record<string, string>,
  requestBody: Buffer,
): Promise<{ status: number; headers: [string, string][]; body: Buffer }> {
  const url = `${scheme}://${host}${path}${query === "" ? "" : `?${query}`}`;
  const method = rawMethod.toUpperCase();
  const init: RequestInit & { duplex?: "half" } = { method, headers };

  if (method !== "GET" && method !== "HEAD" && requestBody.length !== 0) {
    init.body = requestBody;
    init.duplex = "half";
  }

  const nextRequest = new NextRequest(url, init);
  const response = await GET(nextRequest);

  // Refuse to invent a response. A route that returned nothing usable is a
  // failure of the thing under test, and reporting a synthetic 200 here is
  // how the previous version made its own regressions invisible.
  if (response === undefined || response === null) {
    throw new Error("Next route returned no Response");
  }

  const status = response.status;
  if (typeof status !== "number" || status === 0) {
    throw new Error(`Next route returned a Response with no usable status: ${status}`);
  }

  const collected: [string, string][] = [];
  response.headers.forEach((value: string, name: string) => {
    collected.push([name, value]);
  });

  const text = await response.text();
  return { status, headers: collected, body: Buffer.from(text) };
}

type FrameCursor = { frame: Buffer; offset: number };

function readU32(cursor: FrameCursor): number {
  const value = cursor.frame.readUInt32BE(cursor.offset);
  cursor.offset += 4;
  return value;
}

function readText(cursor: FrameCursor): string {
  const length = readU32(cursor);
  const value = cursor.frame.toString("utf8", cursor.offset, cursor.offset + length);
  cursor.offset += length;
  return value;
}

/** Required compact COOP application-library ABI. */
export async function handle(frame: Buffer): Promise<Buffer> {
  if (
    frame.length < 5 ||
    frame[0] !== 0x43 ||
    frame[1] !== 0x4f ||
    frame[2] !== 0x4f ||
    frame[3] !== 0x50 ||
    frame[4] !== 1
  ) {
    throw new Error("invalid Coop HTTP frame");
  }

  const cursor: FrameCursor = { frame, offset: 5 };
  const method = readText(cursor);
  const path = readText(cursor);
  const query = readText(cursor);
  readText(cursor); // remote address is not used by this route
  const scheme = readText(cursor);
  const host = readText(cursor);
  const headerCount = readU32(cursor);
  const headers: Record<string, string> = {};
  for (let index = 0; index < headerCount; index += 1) {
    headers[readText(cursor)] = readText(cursor);
  }
  const bodyLength = readU32(cursor);
  const requestBody = frame.subarray(cursor.offset, cursor.offset + bodyLength);

  const result = await executeNextRoute(
    method,
    path,
    query,
    scheme,
    host,
    headers,
    requestBody,
  );

  let size = 5 + 2 + 4 + 4 + result.body.length;
  const encodedHeaders: [Buffer, Buffer][] = [];
  for (const [name, value] of result.headers) {
    const nameBytes = Buffer.from(name);
    const valueBytes = Buffer.from(value);
    encodedHeaders.push([nameBytes, valueBytes]);
    size += 4 + nameBytes.length + 4 + valueBytes.length;
  }

  const output = Buffer.alloc(size);
  output[0] = 0x43;
  output[1] = 0x4f;
  output[2] = 0x4f;
  output[3] = 0x50;
  output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(result.status, offset);
  offset += 2;
  output.writeUInt32BE(encodedHeaders.length, offset);
  offset += 4;
  for (const [nameBytes, valueBytes] of encodedHeaders) {
    output.writeUInt32BE(nameBytes.length, offset);
    offset += 4;
    nameBytes.copy(output, offset);
    offset += nameBytes.length;
    output.writeUInt32BE(valueBytes.length, offset);
    offset += 4;
    valueBytes.copy(output, offset);
    offset += valueBytes.length;
  }
  output.writeUInt32BE(result.body.length, offset);
  offset += 4;
  result.body.copy(output, offset);
  return output;
}
