// Test 9: regex-heavy — extracts emails + URLs from text.
export function handle(reqJson: string): string {
  const text = "Contact alice@example.com or bob@test.io. Visit https://perch.perryts.com or https://example.org for more. Also email charlie@foo.bar and david@spam.dev today!";

  const emailRe = /[a-z0-9]+@[a-z0-9.]+/g;
  const urlRe = /https?:\/\/[a-z0-9.-]+/g;

  let totalEmails = 0;
  let totalUrls = 0;
  for (let i = 0; i < 100; i++) {
    const emails = text.match(emailRe) || [];
    const urls = text.match(urlRe) || [];
    totalEmails += emails.length;
    totalUrls += urls.length;
  }

  const result = "iterations=100 emails=" + totalEmails + " urls=" + totalUrls;
  const bodyB64 = Buffer.from(result, "utf-8").toString("base64");
  return JSON.stringify({
    status: 200,
    headers: { "content-type": "text/plain" },
    body_base64: bodyB64,
  });
}
