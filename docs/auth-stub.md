# Auth stub contract (part of #7; DTO subset of #22)

`reelarr-types` contains only serde-backed wire types, usable by the WASM UI.
`reelarr-core::auth::AuthService` owns account/session operations; the server's
`AppState` contains `Arc<dyn AuthService>`. The supplied store is **ephemeral**:
restarting forgets the administrator and all sessions. Do not expose this stub to
an untrusted network or use it for production authentication.

## Browser flow

All paths below are under `/api/v1`. `/health` remains public at the root.
Requests and responses use JSON. Same-origin cookies must be included.

1. `GET /auth/setup` → `SetupStatus { needs_setup }`.
2. `GET /auth/session` → `SessionInfo { user: null | { id, username }, csrf_token }`.
   If there is no valid cookie, creates an anonymous bootstrap session. This
   response sets two host-only, `Path=/`, `SameSite=Lax` cookies:
   `reelarr_session` (HttpOnly) and `reelarr_csrf` (readable for double-submit).
3. Every mutation, **including setup, login and logout**, must include both
   cookies and `X-CSRF-Token: <csrf_token>`. Header, CSRF cookie, and the token
   bound to the server-side session must match. Missing/invalid proof returns
   403 `ApiError { code: "csrf_rejected", message }`.
4. `POST /auth/setup` with `SetupRequest { username, password }` creates the
   sole administrator and signs in. Username is 1–128 bytes (not whitespace-only),
   password is 12–1024 bytes. A repeated setup returns 409 `already_configured`.
   Concurrent setup cannot overwrite an existing admin.
5. `POST /auth/login` with `LoginRequest { username, password }` signs in;
   bad credentials return 401 `invalid_credentials`.
6. Both setup and login return `SessionInfo`, replace both cookies, rotate
   session/CSRF tokens and revoke the previous session. Update UI state from
   the response before making the next mutation.
7. `POST /auth/logout` revokes the session and returns a fresh anonymous
   `SessionInfo` and cookies. Other devices' sessions are unaffected.

Session responses have `Cache-Control: no-store`. Sessions and cookies have a
24-hour lifetime. Session identifiers and CSRF tokens are independent random
256-bit values. Passwords are salted Argon2id hashes, computed off the async
executor. Password request structs deliberately do not implement Debug.

## Protected routes and UI composition

`reelarr_server::auth::router()` returns `Router<AppState>` for nesting under
`/api/v1`; the library `app(state)` composes it with health. UI routes can be
merged at the composition boundary and share the same state.

Handlers can extract `AuthUser(UserInfo)`. For groups of protected routes apply
`route_layer(axum::middleware::from_fn_with_state(state.clone(), require_auth))`.
The layer returns 401 `ApiError { code: "unauthorized", message }` for missing,
invalid, expired or anonymous sessions and also enforces CSRF for non-safe
methods. The standalone extractor checks authentication only: pair mutating
handlers with the layer. No placeholder protected route is shipped; integration
tests exercise both boundaries using a test-only route.

## Follow-up hardening (not implemented here)

SQLite account/session persistence, hashed session tokens at rest, setup-mode
lockdown after restart, HTTPS/Secure-cookie configuration, login/setup rate
limits and resource quotas, password policy/recovery, audit logging, and proxy
origin policy follow in #7. Cookies intentionally omit Secure to support the
current local HTTP development server; enable it before HTTPS deployment.
There is no CORS allowance. Anonymous session creation is not rate-limited yet.
