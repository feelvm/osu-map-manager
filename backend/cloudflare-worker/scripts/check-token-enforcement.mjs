#!/usr/bin/env node
// Deploy-time guard: the Worker's client-token gate fails open while the
// CLIENT_TOKEN secret is unset, which leaves /oauth/* and the metadata
// routes open to anonymous callers burning the app's shared osu! quota.
// Refuse to deploy in that state unless the operator explicitly opts out.
import { execSync } from "node:child_process";

if (process.env.ALLOW_TOKENLESS_DEPLOY === "1") {
  console.warn(
    "ALLOW_TOKENLESS_DEPLOY=1: deploying with the X-Client-Token gate OFF.",
  );
  process.exit(0);
}

let output = "";
try {
  output = execSync("npx wrangler secret list", {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
} catch (error) {
  console.error("Could not list Worker secrets (wrangler auth is needed before deploying anyway).");
  if (error && typeof error === "object" && "stderr" in error) {
    console.error(String(error.stderr));
  }
  process.exit(1);
}

if (/CLIENT_TOKEN/.test(output)) {
  console.log("CLIENT_TOKEN secret found: the X-Client-Token gate is enforced.");
  process.exit(0);
}

console.error(
  [
    "The Worker has no CLIENT_TOKEN secret, so the X-Client-Token gate is open:",
    "anyone can call /oauth/* and the metadata routes under this app's osu! quota.",
    "",
    "Enable enforcement, then deploy:",
    "  1. npx wrangler secret put CLIENT_TOKEN   (same value as the repo secret WORKER_CLIENT_TOKEN)",
    "  2. npx wrangler deploy",
    "",
    "Deliberate tokenless deploy: ALLOW_TOKENLESS_DEPLOY=1 npm run deploy",
  ].join("\n"),
);
process.exit(1);
