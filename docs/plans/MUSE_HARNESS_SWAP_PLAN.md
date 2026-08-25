# Muse → jcode Harness Swap (Meta Credits) — Plan

## Goal
Swap your daily driver from `muse` (Muse Spark) to `jcode` while spending the **same Meta-account credits** you already have — i.e. make `jcode login --provider muse` speak the same OAuth subscription auth that `muse login` does, so `jcode --provider muse --model muse-spark-*` burns the same balance, with no separate `META_MUSE_API_KEY` needed.

## Success Criteria
- `jcode auth status` shows `muse: OAuth available` (via Meta device-code login) **or** `muse: imported from Muse CLI` when you already ran `muse login`.
- `jcode --provider muse --model muse-spark-1.2 "hello"` returns a streaming response billed to your Meta account (verified via `jcode auth status --json` + a real model call).
- Existing `meta-muse` API-key path (`META_MUSE_API_KEY` → `https://api.meta.ai/v1`) keeps working unchanged; no regression for `jcode login --provider meta-muse`.
- Model picker `/model` lists `muse-spark-1.2`, `muse-spark-1.1` (and any live catalog additions) under the `muse` provider, with correct 1M context windows.
- No credential copied — jcode reads `~/.config/muse/auth.json` in place after consent, same as it does for Codex/Claude (consent-gated).

## Context And Current Facts

**Your situation:** Credits sit on Meta account (the `muse login` browser flow), not on a `META_MUSE_API_KEY`. Goal is harness swap, not new billing.

**What `muse` actually does (inspected on this machine):**
- Launcher `~/.local/bin/muse` (bash, `strings` + `sed -n` inspection this session):
  - `auth_url = https://auth.meta.com`, `client_id = 1031625952748946`, `authorization_endpoint = /oidc/device/authorization/`, `token_endpoint = /oidc/device/token/`, `grant_type = urn:ietf:params:oauth:grant-type:device_code` — standard OAuth device-code flow.
  - `credential_default = $XDG_CONFIG_HOME/muse/auth.json` else `$HOME/.config/muse/auth.json`, overridable via `MUSE_AUTH_PATH`.
  - On-disk shape `{"providers": {"meta": {"mechanism": "oauth", "access_token": "...", "expires_at": 123}}}` — verified via `read_credential()` in the launcher (`json_member "$document" 0 providers → meta → mechanism == oauth → access_token`).
  - Token is sent as `Authorization: Bearer <token>` (via `download_auth_header` / `ensure_access_token`). TCC blocks `cat ~/.config/muse/auth.json` in this sandbox, but the launcher’s own parser confirms the path/shape.
- `muse login --help` confirms browser device-code; `muse auth set --provider meta --api-key-stdin` confirms the parallel API-key path (`META_API_KEY` priority over OAuth).

**What jcode has today:**
- `crates/jcode-provider-metadata/src/catalog.rs` defines `META_MUSE_PROFILE` as `OpenAiCompatible { id: "meta-muse", api_base: "https://api.meta.ai/v1", env: "META_MUSE_API_KEY", model: "muse-spark-1.2" }` — API-key only.
- `LoginProviderDescriptor` for `meta-muse` is `ApiKey / OpenRouterLike` — no OAuth variant. Dual-auth precedent exists for `claude` (OAuth `sk-ant-oat…` in `~/.jcode/auth.json`) vs `anthropic-api` (API key in `~/.config/jcode/anthropic.env`), and `openai` vs `openai-api` (`crates/jcode-base/src/auth/mod.rs`, `docs/AUTH_CREDENTIAL_SOURCES.md`).
- Provider routing: `crates/jcode-base/src/provider_catalog.rs` maps `"meta-muse"` → `muse-spark-1.2, 1.1` with `1_048_576` context. Models handled as OpenAI-compat via `jcode-provider-openai-runtime`.
- External import pattern: `crates/jcode-base/src/auth/external.rs` + `claude.rs`/`codex.rs`/`cursor.rs` — consent-gated read-in-place of other harnesses’ `auth.json` files, tracked via `Config::allow_external_auth_source_for_path`.

## Constraints And Non-goals

**Constraints:**
- Must not break `meta-muse` API-key users (keep `META_MUSE_API_KEY` + `meta-muse.env`).
- Must reuse jcode’s existing dual-auth + consent model — no new credential store design.
- Token refresh must be explicit: Meta device tokens expire; need polling/refresh without assuming a refresh_token exists (launcher shows only `access_token` + optional `expires_at`; may need re-device-flow).

**Non-goals:**
- No Muse skill / prompt format translation — jcode already drives Muse Spark as OpenAI-compat; keep wire format.
- No Meta-internal billing API integration; credit burn verified indirectly via successful inference + `jcode usage` overlay.
- No Windows Keychain work in v1 (macOS/Linux file path only).

## Key Decisions

| # | Decision | Recommendation | Alternatives Rejected | Why |
|---|----------|----------------|------------------------|-----|
| 1 | Provider identity | **Add new provider `muse` (OAuth)** alongside existing `meta-muse` (API). Mirrors `claude` vs `anthropic-api`, `openai` vs `openai-api`. | Reuse `meta-muse` and bolt OAuth onto it. | Mixing `AuthKind::OAuth` and `ApiKey` on one descriptor breaks `state_for_key`, `auth_status`, TUI login picker ordering, and `provider_catalog` alias resolution. Dual entries is the proven pattern. |
| 2 | Auth flow | **Device-code flow** `POST https://auth.meta.com/oidc/device/authorization/` with `client_id=1031625952748946`, then poll `/oidc/device/token/` (inspected from `muse` launcher). No localhost callback (unlike Claude/OpenAI). | Browser localhost callback (`oauth::wait_for_callback`) | Muse’s launcher *only* implements device-code; no `redirect_uri`/`localhost` listener in its `device_login()`. Don’t invent a different flow that auth.meta.com may not accept for this client_id. |
| 3 | Token storage | `~/.jcode/muse-auth.json` shape `{"muse_accounts": [{"label":"muse-otter","access":"...","expires":123}]}` + `active_muse_account`, mirroring `JcodeAuthFile` (`anthropic_accounts`) and `JcodeOpenAiAuthFile`. Plus consented import from `~/.config/muse/auth.json` (`providers.meta.access_token`). | Store directly in `auth.json` anthropic bucket / reuse `openai-auth.json` | Pollutes the dual-auth accounting and breaks `has_any_available()` bookkeeping. Separate file keeps `muse` vs `claude` accounting clean. |
| 4 | Runtime transport | **Reuse `openai-compatible` runtime** with `Authorization: Bearer <muse_oauth_token>` against `https://api.meta.ai/v1` (same base as `META_MUSE_PROFILE`). Verify base URL by testing OAuth token against `/v1/models` and `/v1/chat/completions`. | New native `jcode-provider-muse-runtime` crate | Unnecessary until we prove Muse’s API diverges from OpenAI-compat (launcher’s download path already uses plain Bearer). Add native crate only if header/response shape demands it. |
| 5 | External import | Add `MuseAuthSource` + `muse.rs` importer reading `~/.config/muse/auth.json` (`providers.meta`) in place, consent-gated via `external_auth::trust_external_auth_source`. | Copy token into jcode store on import | Violates the project’s “never copy external file, read in place after consent” invariant (`docs/AUTH_CREDENTIAL_SOURCES.md`). |
| 6 | Model surface | Keep `muse-spark-1.2/1.1` under both providers; add `muse` provider catalog entry so `/model` shows them under `muse` even without `meta-muse` key. | Hide behind `meta-muse` only | User’s goal is `jcode --provider muse` — picker must list models without requiring the old provider id. |

## Recommended Approach

Phase the work so your harness swap is usable after Phase 1 (API key verified), and your credits flow after Phase 2 (OAuth):

**Phase 0 — Probe (no code, 1–2h, you + agent):**
- On your machine (TCC allows you, not the sandbox), run: `cat ~/.config/muse/auth.json | python3 -m json.tool` and `MUSE_AUTH_PATH=... muse --provider meta --model muse-spark-1.2 "ping"` with `MUSE_LOGIN=1` to confirm device flow still issues `authorization_pending` → `access_token`.
- With a fresh token, `curl -H "Authorization: Bearer $TOKEN" https://api.meta.ai/v1/models` — confirms the OAuth token’s valid *audience* is the same `api.meta.ai` that the API key hits. If it 401s, fallback is `https://api.llama.com/v1` (try both). This single curl decides whether Phase 2 can reuse the existing base URL.

**Phase 1 — OAuth provider + login (core harness swap):**
- Add `MUSE_CLIENT_ID`, `MUSE_AUTH_URL`, `MUSE_DEVICE_AUTH_URL`, `MUSE_DEVICE_TOKEN_URL` constants (`crates/jcode-base/src/auth/oauth.rs` — alongside `claude::` and `openai::` mods).
- New `crates/jcode-base/src/auth/muse.rs` (mirrors `claude.rs`/`codex.rs`): `MUSE_AUTH_SOURCE_ID`, `muse_auth_path()`, `load_muse_oauth_token()`, `save_muse_auth()`, consent helpers `has_unconsented_external_auth()`.
- `crates/jcode-provider-metadata`: new `MUSE_LOGIN_PROVIDER` (`id:"muse"`, `auth_kind: OAuth`, `target: Muse`, `auth_state_key: Muse`) + keep `META_MUSE_LOGIN_PROVIDER` unchanged. Add `LoginProviderTarget::Muse`, `LoginProviderAuthStateKey::Muse`.
- `crates/jcode-base/src/auth/mod.rs`: `AuthStatus { muse: AuthState, muse_has_oauth, muse_has_api }`, `check()` probes `muse.rs`, `state_for_key(Muse)`.
- `crates/jcode-base/src/auth/external.rs` / `crates/jcode-app-core/src/external_auth.rs`: wire Muse detection into `unconsented_sources` + TUI onboarding “Found Muse credentials at ~/.config/muse/auth.json — trust?” card.
- `crates/jcode-base/src/auth/login_flows.rs`: `muse_device_login()` — `POST /oidc/device/authorization` → print `user_code` + `verification_uri_complete` → poll `/oidc/device/token` with backoff on `slow_down`/`authorization_pending` (exact logic from `device_login()` in the launcher, `interval` default 5s, `lifetime` 900s).
- `crates/jcode-base/src/provider_catalog.rs` + `crates/jcode-provider-core/src/models.rs`: add `"muse"` branch (same models as `meta-muse`) and `provider_for_model("muse-spark-*") → "muse"`.

**Phase 2 — Transport + model routing:**
- Extend `MultiProvider` / `AuthStatus::resolve_active_provider` to prefer `muse` OAuth token when `provider == "muse"` or when routing `muse-spark-*` without `META_MUSE_API_KEY`. Resolve as `Bearer <muse_access>` — no `x-api-key` variant.
- Expose via CLI: `jcode login --provider muse` (device code), `jcode logout --provider muse`, `jcode auth status --provider muse`.
- Keep `meta-muse` routing untouched; add `provider_init.rs: ProviderChoice::Muse`.

**Phase 3 — Polish (optional, post-swap):**
- Token refresh / expiry UX: if `expires_at` in past, background thread re-polls or re-prompts device flow; surface “expired — run `jcode login --provider muse`” (same as Claude `AuthState::Expired`).
- Usage overlay: ensure `jcode-usage-types` attributes Muse token usage (Meta bills per-token; no special pricing table needed beyond existing OpenAI-compat passthrough).
- Doctor: `jcode provider-doctor --provider muse` live probes `/v1/models` + `chat/completions` with the OAuth token.

## Work Plan

| Step | Owner/Surface | Description | Dependency |
|------|---------------|-------------|------------|
| 0.1 | You (local) | Paste `~/.config/muse/auth.json` shape (redact token) + curl `api.meta.ai/v1/models` with OAuth Bearer result | None |
| 1.1 | `oauth.rs` | Add `pub mod muse { CLIENT_ID=1031625952748946, AUTH_URL=https://auth.meta.com, AUTH_ENDPOINT=/oidc/device/authorization/, TOKEN_ENDPOINT=/oidc/device/token/, GRANT=device_code }` | — |
| 1.2 | `auth/muse.rs` (new) | Implement file I/O for `~/.jcode/muse-auth.json` + `~/.config/muse/auth.json` read helpers; tests mirror `claude_tests.rs` | 1.1 |
| 1.3 | `provider-metadata` | Add `MUSE_LOGIN_PROVIDER`, `LoginProviderTarget::Muse`, `AuthStateKey::Muse`; bump `LOGIN_PROVIDERS` len | 1.2 |
| 1.4 | `auth/mod.rs` | `AuthStatus` fields + `check()`/`check_fast()` + `log_snapshot()` for `muse` | 1.3 |
| 1.5 | `external.rs` + `external_auth.rs` | Consent-gated import of `~/.config/muse/auth.json`; TUI onboarding card | 1.4 |
| 1.6 | `login_flows.rs` + CLI | `jcode login --provider muse` device flow (reuse `device_login()` polling loop) | 1.1 |
| 2.1 | `provider_catalog.rs` + `models.rs` | `muse` catalog branch + `provider_for_model` | 1.3 |
| 2.2 | `provider` routing | `MultiProvider` resolves `muse` OAuth Bearer for `muse-spark-*`; no fallback to API key when `provider==muse` | 1.4, 2.1 |
| 2.3 | `provider_init.rs` + TUI | `ProviderChoice::Muse`, startup `default_provider=muse` support, picker ordering | 2.2 |
| 3.1 | Doctor/validation | `validation.rs` stale handling + `provider-doctor` live probe for `muse` | 2.2 |
| 3.2 | Docs | Update `AUTH_CREDENTIAL_SOURCES.md` + `README.md` (`jcode login --provider muse`) | All |

**PR split:** 1 PR for Phase 1 (auth+login+import), 1 PR for Phase 2 (transport+routing). Phase 3 docs/doctor can ride with Phase 2 if small.

## Validation Plan

- **Unit (each step):**
  - `cargo test -p jcode-base --lib auth::muse -- --nocapture` — parse `~/.config/muse/auth.json` fixture, `providers.meta.access_token` extraction, expiry handling.
  - `cargo test -p jcode-provider-metadata --lib` — `MUSE_LOGIN_PROVIDER` appears in `cli_login_providers()` at expected order (after `claude`/`openai`, before `meta-muse`), aliases `muse`/`muse-spark` resolve.
  - `cargo test -p jcode-base --lib provider_catalog` — `provider_for_model("muse-spark-1.2") == Some("muse")`, context window `1_048_576`.
- **Integration (Phase 1):**
  - `jcode auth status --json | jq .muse` — before login: `not_configured`; after `muse login`: `available` via external import; after `jcode login --provider muse`: `available` via `~/.jcode/muse-auth.json`.
  - `jcode login --provider muse` in interactive terminal — expect device-code URL + `user_code` printed to stderr, browser opens `https://auth.meta.com/...`, token saved.
  - `jcode logout --provider muse && jcode auth status --json` — returns to `not_configured`; external file still on disk but untrusted until re-approved.
- **E2E (Phase 2 — the harness swap proof):**
  - `jcode exec --provider muse --model muse-spark-1.2 "Say hello in one word"` — must stream and exit 0, billed to Meta credits (check Meta dashboard; no `META_MUSE_API_KEY` in env).
  - `jcode --provider muse` TUI → `/model` → `muse-spark-1.2` selectable without `meta-muse` key; `/account` shows `muse: OAuth via Meta (device_code)`.
  - Regression: `META_MUSE_API_KEY=test-key jcode auth status --json | jq '.["meta-muse"]'` still `available`; `jcode exec --provider meta-muse --model muse-spark-1.2` still works.

## Risks / Rollback

- **Token audience mismatch** (OAuth token not valid for `api.meta.ai/v1`): Mitigated by Phase 0 curl; if it fails, switch base to `https://api.llama.com/compat/v1` (try before writing code). Rollback: keep `muse` provider disabled in `LOGIN_PROVIDERS` until verified.
- **No refresh_token issued** (launcher only shows `access_token`): Expiry would force re-login. Mitigation: store `expires_at`, surface `Expired` state, and on 401 retry re-device-flow automatically — same as Claude’s `AuthState::Expired` path. No silent retry loop.
- **TCC / `~/.config/muse/auth.json` unreadable in sandbox CI**: Tests use fixtures, not live file; live import only exercised locally with consent. CI not blocked.
- **Rollback:** `git revert` Phase 1/2 commits; `muse` provider disappears from `auth status`, but `~/.jcode/muse-auth.json` + `~/.config/muse/auth.json` remain intact. No data loss.

## Open Questions

- None blocking — the launcher inspection answered the OAuth URL/client_id/shape questions. One to confirm live with you before coding:
  - Does `curl -H "Authorization: Bearer <your Muse OAuth token>" https://api.meta.ai/v1/models` return 200? If not, what’s the correct base URL your token is scoped to? Your answer picks the single constant that Phase 2 needs.

---
*Teams responsible: `jcode-base` (auth), `jcode-provider-metadata` (catalog), `jcode-app-core` (login flows / external_auth), `jcode-tui` (picker), `jcode-provider-core` (model routing). Plan file: `~/git/jcode/docs/plans/MUSE_HARNESS_SWAP_PLAN.md`.*
