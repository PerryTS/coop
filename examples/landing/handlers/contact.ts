// Landing page contact form handler.
//
// Receives POST /contact with form data (email + message), logs the
// submission, and returns a 303 redirect to /thanks.html.
//
// For v0 this is a self-contained handler that uses inline @perch/runtime
// helpers (the import mechanism for local packages via Perry is TBD).
// The handler shape matches the v0.5 wire protocol: exported `handle`
// function that takes a DeploymentRequest JSON string and returns a
// DeploymentResponse JSON string.

export function handle(reqJson: string): string {
  const raw = JSON.parse(reqJson);
  const method = raw.method || "GET";
  const path = raw.path || "/";

  // Decode base64 body to text.
  let bodyText = "";
  if (raw.body_base64 && raw.body_base64 !== "") {
    bodyText = Buffer.from(raw.body_base64, "base64").toString("utf-8");
  }

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
    return JSON.stringify({
      status: 303,
      headers: { "location": "/thanks.html" },
      body_base64: "",
    });
  }

  // Default: 404 for unmatched routes.
  const body404 = Buffer.from("Not Found", "utf-8").toString("base64");
  return JSON.stringify({
    status: 404,
    headers: { "content-type": "text/plain" },
    body_base64: body404,
  });
}
