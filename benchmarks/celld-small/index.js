export default {
  async fetch(request) {
    const url = new URL(request.url);
    const requested = Number(url.searchParams.get("iterations") ?? "100");
    const iterations = Number.isFinite(requested)
      ? Math.max(1, Math.min(10_000, Math.trunc(requested)))
      : 100;

    let checksum = 0x811c9dc5;
    for (let index = 0; index < iterations; index += 1) {
      checksum = Math.imul(checksum ^ index, 0x01000193) >>> 0;
    }

    return Response.json({
      runtime: "celld",
      iterations,
      checksum,
    });
  },
};
