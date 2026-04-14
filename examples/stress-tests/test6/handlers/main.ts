// Test 6: string-heavy work — split, transform, join.
export function handle(reqJson: string): string {
  const lorem = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua";
  let text = "";
  for (let i = 0; i < 50; i++) {
    text += lorem + " ";
  }

  const words = text.split(" ");
  const upper = words.map((w: string) => w.toUpperCase());
  const filtered = upper.filter((w: string) => w.length > 4);
  const joined = filtered.join("|");

  const result = "words=" + words.length + " filtered=" + filtered.length + " sample=" + joined.substring(0, 200);
  const bodyB64 = Buffer.from(result, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "text/plain" },
    body_base64: bodyB64,
  });
}
