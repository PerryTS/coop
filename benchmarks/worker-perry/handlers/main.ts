type FrameCursor = { frame: Buffer; offset: number };

function readU32(cursor: FrameCursor): number {
  if (cursor.offset + 4 > cursor.frame.length) {
    throw new Error("truncated COOP length");
  }
  const value = cursor.frame.readUInt32BE(cursor.offset);
  cursor.offset += 4;
  return value;
}

function readText(cursor: FrameCursor): string {
  const length = readU32(cursor);
  if (cursor.offset + length > cursor.frame.length) {
    throw new Error("truncated COOP field");
  }
  const value = cursor.frame.toString("utf8", cursor.offset, cursor.offset + length);
  cursor.offset += length;
  return value;
}

function benchmarkBody(query: string): Buffer {
  // The cross-runtime oracle always sends iterations=100. Read and validate
  // that value so a routing/query regression cannot silently execute less
  // work, while avoiding framework-specific URL APIs in this mechanism test.
  if (query !== "iterations=100") {
    throw new Error("unexpected benchmark query");
  }
  const iterations = 100;
  let checksum = 0x811c9dc5;
  for (let index = 0; index < iterations; index += 1) {
    checksum = Math.imul(checksum ^ index, 0x01000193) >>> 0;
  }
  return Buffer.from(JSON.stringify({
    runtime: "perry",
    iterations,
    checksum,
  }));
}

function responseFrame(body: Buffer): Buffer {
  const headerName = Buffer.from("content-type");
  const headerValue = Buffer.from("application/json");
  const output = Buffer.alloc(
    5 + 2 + 4 + 4 + headerName.length + 4 + headerValue.length + 4 + body.length,
  );
  output[0] = 0x43;
  output[1] = 0x4f;
  output[2] = 0x4f;
  output[3] = 0x50;
  output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(200, offset);
  offset += 2;
  output.writeUInt32BE(1, offset);
  offset += 4;
  output.writeUInt32BE(headerName.length, offset);
  offset += 4;
  headerName.copy(output, offset);
  offset += headerName.length;
  output.writeUInt32BE(headerValue.length, offset);
  offset += 4;
  headerValue.copy(output, offset);
  offset += headerValue.length;
  output.writeUInt32BE(body.length, offset);
  offset += 4;
  body.copy(output, offset);
  return output;
}

/** Strict binary HTTP ABI used by the equivalent Worker mechanism matrix. */
export function handle(frame: Buffer): Buffer {
  if (
    frame.length < 5 ||
    frame[0] !== 0x43 ||
    frame[1] !== 0x4f ||
    frame[2] !== 0x4f ||
    frame[3] !== 0x50 ||
    frame[4] !== 1
  ) {
    throw new Error("invalid COOP HTTP request");
  }

  const cursor: FrameCursor = { frame, offset: 5 };
  readText(cursor); // method
  readText(cursor); // path
  const query = readText(cursor);
  readText(cursor); // remote address
  readText(cursor); // scheme
  readText(cursor); // host
  const headerCount = readU32(cursor);
  for (let index = 0; index < headerCount; index += 1) {
    readText(cursor);
    readText(cursor);
  }
  const bodyLength = readU32(cursor);
  cursor.offset += bodyLength;
  if (cursor.offset !== frame.length) {
    throw new Error("trailing or truncated COOP bytes");
  }
  return responseFrame(benchmarkBody(query));
}
