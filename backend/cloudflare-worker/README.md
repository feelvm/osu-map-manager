# osu! Map Manager Worker

Cloudflare Worker backend for osu! sign-in and metadata lookups. The desktop
app calls these Worker endpoints:

```text
GET  /oauth/authorize          (opened in the user's browser; top-level navigation)
GET  /oauth/check
POST /oauth/token
POST /oauth/refresh
GET  /beatmapsets/{beatmapset_id}
GET  /beatmaps/{beatmap_id}
```

There is no download route: the desktop app downloads beatmapsets directly
from the official osu! API (with the signed-in user's token) or from the
public mirror, so a proxied download endpoint would only serve third parties.

## Setup

Deploy the Worker:

```powershell
cd backend/cloudflare-worker
npm install
npx wrangler deploy
```

Put the deployed Worker URL into the desktop app's `Backend URL` field, for example:

```text
https://osu-map-manager.YOUR_SUBDOMAIN.workers.dev
```

## OAuth credentials

Create an OAuth application at <https://osu.ppy.sh/home/account/edit#oauth> with
redirect URI `http://127.0.0.1:3000/callback`, then store its credentials on the
Worker (never in the app):

```powershell
npx wrangler secret put OSU_CLIENT_ID
npx wrangler secret put OSU_CLIENT_SECRET
npx wrangler deploy
```

`GET /oauth/check` verifies the stored credentials live against osu!; the
desktop app calls it before opening the browser so a bad secret fails fast.

## Troubleshooting

`invalid_client` / `Client authentication failed` means osu! rejected the
request. Where it happens matters:

- **In the browser on `/oauth/authorize` (HTTP 401):** osu! does not accept the
  `client_id` + `redirect_uri` combination. Either the OAuth application was
  deleted, or — more commonly — its registered callback URL does not exactly
  match what the app sends. Open the app at
  <https://osu.ppy.sh/home/account/edit#oauth> and set its Application Callback
  URL to exactly:
  ```text
  http://127.0.0.1:3000/callback
  ```
  No trailing slash, no extra path, `http` (not `https`), port `3000`.
- **At the token exchange (`/oauth/token`):** the stored
  `OSU_CLIENT_ID`/`OSU_CLIENT_SECRET` pair is wrong. Common causes:

- The values were swapped, truncated, or pasted with extra characters.
- The secret was regenerated on osu! after being stored on the Worker.
- The secrets were set on a different Worker/environment than the deployed one.

Fix: re-enter both secrets with the commands above (paste the numeric client ID
and the exact client secret, no quotes), redeploy, then sign in again.

## Security model

Do not put private credentials into the desktop app. Any value embedded in a
distributed EXE can be extracted.

The desktop app signs the user in with osu! OAuth through this Worker
(`GET /oauth/authorize`, `POST /oauth/token`, `POST /oauth/refresh`); the
Worker holds `OSU_CLIENT_ID`/`OSU_CLIENT_SECRET` and the browser-facing app
only ever sees short-lived user tokens.

Sign-in uses PKCE (RFC 7636): the app sends `code_challenge`/`code_challenge_method=S256`
to `GET /oauth/authorize` and `code_verifier` to `POST /oauth/token`. Requests
without them are rejected, so an intercepted authorization code is useless on
its own.

Metadata routes (`GET /beatmapsets/:id`, `GET /beatmaps/:id`) prefer the
caller's own osu! token when an `Authorization: Bearer` header is present and
fall back to the Worker's app token otherwise.

## Abuse gates

Every route except `/health` and `/oauth/authorize` (the one route the user's
browser legitimately opens) is restricted to the desktop app:

- **Browser requests are rejected.** Browsers attach `Origin` and
  `Sec-Fetch-Site` to requests; plain HTTP clients like the app send neither,
  so any request carrying them gets HTTP 403. This stops websites from
  driving the Worker through visitors' browsers.
- **Per-IP rate limits** (Cloudflare rate-limiting bindings, keyed on
  `CF-Connecting-IP` at the edge): 30 req/min on the OAuth routes, 600 req/min
  on the metadata routes. A limiter error fails open so a binding hiccup
  cannot take sign-in down.
- **Optional shared client token.** While the `CLIENT_TOKEN` secret is set,
  API routes additionally require `X-Client-Token` to match:

  ```powershell
  npx wrangler secret put CLIENT_TOKEN
  npx wrangler deploy
  ```

  Set it to the same value as the repository secret
  `WORKER_CLIENT_TOKEN`, which the release workflow compiles into the app
  (`OSU_MAP_MANAGER_CLIENT_TOKEN`). Until the secret is set the Worker accepts
  tokenless clients, so enforcement can be switched on after a token-carrying
  release exists. A token embedded in a public EXE can be extracted by a
  determined attacker — it raises the bar against casual abuse, it is not a
  secret in the strong sense.

The Worker also caches the osu! access token in memory until close to expiry
to avoid requesting a new token per metadata burst.

## Rate posture

osu! asks API clients to stay under 60 requests/minute per token. The design
respects that on three levels:

- Successful metadata responses (`GET /beatmapsets/:id`, `GET /beatmaps/:id`)
  are cached by URL for 5 minutes across all callers — the bodies are public
  and identical regardless of whose token fetched them.
- When the desktop app is signed in, metadata and downloads spend the *user's*
  own quota; the shared app quota is only spent by unsigned callers (who the
  app additionally paces to ~1 req/s).
- Nothing retries: a failed check or download is reported once, never looped.
