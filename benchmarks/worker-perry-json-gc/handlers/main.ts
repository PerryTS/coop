// Minimal host-boundary full-GC correctness witness. Request parsing is
// intentionally absent: the only semantic step beyond the COOP envelope is
// JSON.stringify of a fresh object followed by Buffer.from.
export function handle(_frame: Buffer): Buffer {
  const body = Buffer.from(JSON.stringify({
    runtime: "perry",
    iterations: 100,
    checksum: 3726872593,
  }));
  const output = Buffer.alloc(5 + 2 + 4 + 4 + body.length);
  output[0] = 0x50;
  output[1] = 0x43;
  output[2] = 0x48;
  output[3] = 0x32;
  output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(200, offset);
  offset += 2;
  output.writeUInt32BE(0, offset);
  offset += 4;
  output.writeUInt32BE(body.length, offset);
  offset += 4;
  body.copy(output, offset);
  return output;
}
