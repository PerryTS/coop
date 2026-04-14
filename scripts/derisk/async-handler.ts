// Verify: does Perry compile `async function handle()` to a function
// that returns a Promise pointer? What's the symbol name?

export async function handle(reqJson: string): Promise<string> {
  // Use setTimeout to force async — proves the handler actually awaits.
  await new Promise<void>((resolve) => setTimeout(resolve, 1));
  return "async returned: " + reqJson;
}
