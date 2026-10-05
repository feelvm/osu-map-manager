export interface Env {
  OSU_CLIENT_ID: string;
  OSU_CLIENT_SECRET: string;
  OSU_TOKEN_URL: string;
  OSU_API_BASE_URL: string;
  BEATMAP_MIRROR_DOWNLOAD_BASE_URL: string;
}

type CachedToken = {
  value: string;
  expiresAt: number;
};

type OAuthSecrets = {
  clientId: string;
  clientSecret: string;
};

let cachedToken: CachedToken | undefined;

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    // No CORS headers on purpose: the only client is the desktop app
    // (non-browser, unaffected by CORS), so cross-origin browser access is
    // denied by default instead of letting any website drive the Worker.
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204 });
    }

    const url = new URL(request.url);
    if (url.pathname === "/health") {
      return json({ ok: true });
    }

    if (request.method === "GET" && url.pathname === "/oauth/authorize") {
      return authorizeOsu(url, env);
    }

    if (request.method === "GET" && url.pathname === "/oauth/check") {
      return checkOAuth(env);
    }

    if (request.method === "POST" && url.pathname === "/oauth/token") {
      return exchangeAuthorizationCode(request, env);
    }

    if (request.method === "POST" && url.pathname === "/oauth/refresh") {
      return refreshUserToken(request, env);
    }

    const downloadMatch = url.pathname.match(/^\/beatmapsets\/(\d+)\/download$/);
    if (request.method === "GET" && downloadMatch) {
      return downloadBeatmapset(Number(downloadMatch[1]), env, request);
    }

    const beatmapsetMatch = url.pathname.match(/^\/beatmapsets\/(\d+)$/);
    if (request.method === "GET" && beatmapsetMatch) {
      return getBeatmapset(Number(beatmapsetMatch[1]), env, request, ctx);
    }

    const beatmapMatch = url.pathname.match(/^\/beatmaps\/(\d+)$/);
    if (request.method === "GET" && beatmapMatch) {
      return getBeatmap(Number(beatmapMatch[1]), env, request, ctx);
    }

    return json({ error: "not_found" }, 404);
  }
};

async function checkOAuth(env: Env): Promise<Response> {
  const secrets = getOAuthSecrets(env);
  if (!secrets.ok) {
    return json(secrets.body, 500);
  }

  // Verify the credentials live against osu! so the desktop app can fail fast
  // with an actionable message instead of failing at the token exchange.
  // The secret itself is never included in any response.
  try {
    const response = await fetch(env.OSU_TOKEN_URL, {
      method: "POST",
      headers: {
        "Accept": "application/json",
        "Content-Type": "application/x-www-form-urlencoded"
      },
      body: new URLSearchParams({
        client_id: secrets.value.clientId,
        client_secret: secrets.value.clientSecret,
        grant_type: "client_credentials",
        scope: "public"
      })
    });

    if (!response.ok) {
      const detail = await safeJson(response);
      if (response.status === 401 || detail.error === "invalid_client") {
        return json({
          ok: false,
          error: "invalid_client",
          message: "osu! rejected OSU_CLIENT_ID/OSU_CLIENT_SECRET. Re-set them on the Worker with `npx wrangler secret put OSU_CLIENT_ID` and `npx wrangler secret put OSU_CLIENT_SECRET` using the values from https://osu.ppy.sh/home/account/edit#oauth, then redeploy."
        }, 500);
      }
      return json({
        ok: false,
        error: "osu_unreachable",
        message: `osu! token endpoint returned HTTP ${response.status}`
      }, 502);
    }

    return json({ ok: true });
  } catch (error) {
    return json({ ok: false, error: "osu_unreachable", message: String(error) }, 502);
  }
}

function authorizeOsu(url: URL, env: Env): Response {
  const secrets = getOAuthSecrets(env);
  if (!secrets.ok) {
    return json(secrets.body, 500);
  }

  const redirectUri = url.searchParams.get("redirect_uri") ?? "";
  const state = url.searchParams.get("state") ?? "";
  const codeChallenge = url.searchParams.get("code_challenge") ?? "";
  const codeChallengeMethod = url.searchParams.get("code_challenge_method") ?? "";
  if (!isAllowedLoopbackRedirect(redirectUri)) {
    return json({ error: "invalid_redirect_uri" }, 400);
  }
  if (!state) {
    return json({ error: "missing_state" }, 400);
  }
  // PKCE (RFC 7636) is mandatory: an intercepted authorization code is
  // useless without the verifier, which never passes through the browser.
  if (!isValidCodeChallenge(codeChallenge) || codeChallengeMethod !== "S256") {
    return json({ error: "missing_code_challenge" }, 400);
  }

  const authorize = new URL("https://osu.ppy.sh/oauth/authorize");
  authorize.searchParams.set("client_id", secrets.value.clientId);
  authorize.searchParams.set("redirect_uri", redirectUri);
  authorize.searchParams.set("response_type", "code");
  authorize.searchParams.set("scope", "public");
  authorize.searchParams.set("state", state);
  authorize.searchParams.set("code_challenge", codeChallenge);
  authorize.searchParams.set("code_challenge_method", "S256");
  return Response.redirect(authorize.toString(), 302);
}

function isValidCodeChallenge(value: string): boolean {
  return value.length >= 43 && value.length <= 128 && /^[A-Za-z0-9\-._~]+$/.test(value);
}

async function exchangeAuthorizationCode(request: Request, env: Env): Promise<Response> {
  const secrets = getOAuthSecrets(env);
  if (!secrets.ok) {
    return json(secrets.body, 500);
  }

  const form = await request.formData();
  const code = String(form.get("code") ?? "");
  const redirectUri = String(form.get("redirect_uri") ?? "");
  const codeVerifier = String(form.get("code_verifier") ?? "");
  if (!code) {
    return json({ error: "missing_code" }, 400);
  }
  if (!isAllowedLoopbackRedirect(redirectUri)) {
    return json({ error: "invalid_redirect_uri" }, 400);
  }
  if (!codeVerifier) {
    return json({ error: "missing_code_verifier" }, 400);
  }

  return proxyTokenRequest({
    client_id: secrets.value.clientId,
    client_secret: secrets.value.clientSecret,
    grant_type: "authorization_code",
    code,
    redirect_uri: redirectUri,
    code_verifier: codeVerifier
  }, env);
}

async function refreshUserToken(request: Request, env: Env): Promise<Response> {
  const secrets = getOAuthSecrets(env);
  if (!secrets.ok) {
    return json(secrets.body, 500);
  }

  const form = await request.formData();
  const refreshToken = String(form.get("refresh_token") ?? "");
  if (!refreshToken) {
    return json({ error: "missing_refresh_token" }, 400);
  }

  return proxyTokenRequest({
    client_id: secrets.value.clientId,
    client_secret: secrets.value.clientSecret,
    grant_type: "refresh_token",
    refresh_token: refreshToken
  }, env);
}

async function proxyTokenRequest(values: Record<string, string>, env: Env): Promise<Response> {
  const response = await fetch(env.OSU_TOKEN_URL, {
    method: "POST",
    headers: {
      "Accept": "application/json",
      "Content-Type": "application/x-www-form-urlencoded"
    },
    body: new URLSearchParams(values)
  });

  const body = await response.text();
  return new Response(body, {
    status: response.status,
    headers: {
      "Content-Type": response.headers.get("Content-Type") ?? "application/json; charset=utf-8",
      "Cache-Control": "no-store"
    }
  });
}

function getOAuthSecrets(env: Env): { ok: true; value: OAuthSecrets } | { ok: false; body: unknown } {
  const clientId = normalizeSecret(env.OSU_CLIENT_ID);
  const clientSecret = normalizeSecret(env.OSU_CLIENT_SECRET);
  if (!clientId || !clientSecret) {
    return {
      ok: false,
      body: {
        error: "oauth_not_configured",
        message: "OSU_CLIENT_ID and OSU_CLIENT_SECRET must be configured on the Worker"
      }
    };
  }
  if (!/^\d+$/.test(clientId)) {
    return {
      ok: false,
      body: {
        error: "invalid_oauth_client_id",
        message: "OSU_CLIENT_ID should be the numeric osu! OAuth client id"
      }
    };
  }
  return { ok: true, value: { clientId, clientSecret } };
}

function normalizeSecret(value: string | undefined): string {
  const trimmed = String(value ?? "").trim();
  if (
    (trimmed.startsWith("\"") && trimmed.endsWith("\""))
    || (trimmed.startsWith("'") && trimmed.endsWith("'"))
  ) {
    return trimmed.slice(1, -1).trim();
  }
  return trimmed;
}

function isAllowedLoopbackRedirect(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "http:"
      && url.hostname === "127.0.0.1"
      && url.port === "3000"
      && url.pathname === "/callback";
  } catch {
    return false;
  }
}

/// Extracts a caller-provided osu! user token, if any. It is forwarded as-is
/// and never logged or stored.
function userBearer(request: Request): string | undefined {
  const header = request.headers.get("Authorization");
  if (header && /^bearer\s+\S+$/i.test(header.trim())) {
    return header.trim();
  }
  return undefined;
}

const WORKER_USER_AGENT = "osu-map-manager/0.1 (+https://osu-map-manager.stanislavberman.workers.dev)";

// Metadata requests prefer the caller's own osu! token (their quota, their
// rate limit) and fall back to the Worker's app token when absent or
// rejected, so anonymous traffic cannot silently burn the app quota.
async function osuApiGet(path: string, env: Env, request: Request): Promise<Response> {
  const user = userBearer(request);
  if (user) {
    const response = await fetch(`${env.OSU_API_BASE_URL}${path}`, {
      headers: {
        "Accept": "application/json",
        "Authorization": user,
        "User-Agent": WORKER_USER_AGENT
      }
    });
    if (response.status !== 401 && response.status !== 403) {
      return response;
    }
    // Expired or unauthorized user token: fall through to the app token.
  }

  const token = await getAccessToken(env);
  return fetch(`${env.OSU_API_BASE_URL}${path}`, {
    headers: {
      "Accept": "application/json",
      "Authorization": `Bearer ${token}`,
      "User-Agent": WORKER_USER_AGENT
    }
  });
}

/// Public osu! metadata is identical regardless of whose token fetched it,
/// so successful responses are cached by URL across all callers for a few
/// minutes. Repeat and burst checks are absorbed here before they reach
/// osu!, keeping the shared app quota far under the documented 60 req/min.
/// (A few minutes of staleness is harmless for weekly update checks.)
const METADATA_CACHE_TTL_SECONDS = 300;

async function cachedOsuApiGet(
  path: string,
  env: Env,
  request: Request,
  ctx: ExecutionContext
): Promise<Response> {
  const cacheKey = new Request(`${env.OSU_API_BASE_URL}${path}`, { method: "GET" });
  let cache: Cache | undefined;
  try {
    cache = await caches.open("osu-metadata");
    const cached = await cache.match(cacheKey);
    if (cached) {
      return cached;
    }
  } catch {
    // Cache unavailable: fall through to upstream.
  }

  const upstream = await osuApiGet(path, env, request);
  if (upstream.ok && cache) {
    const copy = upstream.clone();
    const headers = new Headers(copy.headers);
    headers.set("Cache-Control", `public, max-age=${METADATA_CACHE_TTL_SECONDS}`);
    ctx.waitUntil(
      cache.put(cacheKey, new Response(copy.body, { status: copy.status, headers }))
    );
  }
  return upstream;
}

async function getBeatmapset(beatmapsetId: number, env: Env, request: Request, ctx: ExecutionContext): Promise<Response> {
  if (!Number.isSafeInteger(beatmapsetId) || beatmapsetId <= 0) {
    return json({ error: "invalid_beatmapset_id" }, 400);
  }

  try {
    const upstream = await cachedOsuApiGet(`/beatmapsets/${beatmapsetId}`, env, request, ctx);

    if (!upstream.ok) {
      const body = await safeText(upstream);
      return json({ error: "osu_metadata_failed", status: upstream.status, body }, upstream.status);
    }

    return json(await upstream.json());
  } catch (error) {
    return json({ error: "osu_metadata_failed", message: String(error) }, 502);
  }
}

async function getBeatmap(beatmapId: number, env: Env, request: Request, ctx: ExecutionContext): Promise<Response> {
  if (!Number.isSafeInteger(beatmapId) || beatmapId <= 0) {
    return json({ error: "invalid_beatmap_id" }, 400);
  }

  try {
    const upstream = await cachedOsuApiGet(`/beatmaps/${beatmapId}`, env, request, ctx);

    if (upstream.status === 404) {
      return json({ error: "beatmap_not_found" }, 404);
    }
    if (!upstream.ok) {
      const body = await safeText(upstream);
      return json({ error: "osu_metadata_failed", status: upstream.status, body }, upstream.status);
    }

    return json(await upstream.json());
  } catch (error) {
    return json({ error: "osu_metadata_failed", message: String(error) }, 502);
  }
}

async function downloadBeatmapset(beatmapsetId: number, env: Env, request: Request): Promise<Response> {
  if (!Number.isSafeInteger(beatmapsetId) || beatmapsetId <= 0) {
    return json({ error: "invalid_beatmapset_id" }, 400);
  }

  // When the desktop app is signed in with osu! OAuth, prefer the official
  // osu! API download endpoint. The user's bearer token is forwarded as-is
  // and never logged or stored. Falls back to the mirror below when there is
  // no token or the official download fails.
  const authorization = request.headers.get("Authorization");
  if (authorization && /^bearer\s+\S+$/i.test(authorization.trim())) {
    try {
      const official = await fetch(`${env.OSU_API_BASE_URL}/beatmapsets/${beatmapsetId}/download`, {
        headers: {
          "Accept": "application/octet-stream",
          "Authorization": authorization.trim(),
          "User-Agent": "osu-map-manager/0.1"
        }
      });
      if (official.ok && official.body) {
        return proxiedDownload(official, beatmapsetId, "osu-api");
      }
      // Fall through to the mirror on auth/availability failures so repair
      // still works (e.g. expired token, missing supporter status).
    } catch {
      // Fall through to the mirror.
    }
  }

  const upstream = await fetch(`${env.BEATMAP_MIRROR_DOWNLOAD_BASE_URL}/${beatmapsetId}`, {
    headers: {
      "Accept": "application/octet-stream",
      // catboy.best rejects user agents it cannot classify as a browser,
      // so present one (a plain app UA gets a 403).
      "User-Agent":
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
    }
  });

  if (!upstream.ok || !upstream.body) {
    const body = await safeText(upstream);
    return json({ error: "mirror_download_failed", status: upstream.status, body }, upstream.status);
  }

  return proxiedDownload(upstream, beatmapsetId, "mirror");
}

function proxiedDownload(upstream: Response, beatmapsetId: number, source: string): Response {
  const headers = new Headers(upstream.headers);
  headers.set("Content-Disposition", `attachment; filename="${beatmapsetId}.osz"`);
  headers.set("Cache-Control", "no-store");
  headers.set("X-Download-Source", source);
  return new Response(upstream.body, {
    status: upstream.status,
    headers
  });
}

async function getAccessToken(env: Env): Promise<string> {
  const secrets = getOAuthSecrets(env);
  if (!secrets.ok) {
    throw new Error(JSON.stringify(secrets.body));
  }

  const now = Math.floor(Date.now() / 1000);
  if (cachedToken && cachedToken.expiresAt - 60 > now) {
    return cachedToken.value;
  }

  const body = new URLSearchParams({
    client_id: secrets.value.clientId,
    client_secret: secrets.value.clientSecret,
    grant_type: "client_credentials",
    scope: "public"
  });

  const response = await fetch(env.OSU_TOKEN_URL, {
    method: "POST",
    headers: {
      "Accept": "application/json",
      "Content-Type": "application/x-www-form-urlencoded"
    },
    body
  });

  if (!response.ok) {
    throw new Error(`osu token request failed: ${response.status} ${await safeText(response)}`);
  }

  const token = await response.json<{
    access_token: string;
    expires_in: number;
  }>();

  cachedToken = {
    value: token.access_token,
    expiresAt: now + token.expires_in
  };

  return cachedToken.value;
}

function json(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: {
      "Content-Type": "application/json; charset=utf-8",
      "Cache-Control": "no-store"
    }
  });
}

async function safeText(response: Response): Promise<string> {
  try {
    return (await response.text()).slice(0, 500);
  } catch {
    return "";
  }
}

async function safeJson(response: Response): Promise<{ error?: string }> {
  try {
    return await response.json();
  } catch {
    return {};
  }
}
