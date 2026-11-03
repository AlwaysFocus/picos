# ChatGPT subscription authentication

picos strict mode uses its own Rust harness and a separate Sign in with ChatGPT grant. It does not launch Codex, import Codex credentials, use a private ChatGPT inference endpoint, or fall back to API-key billing.

OpenAI documents this direct integration for open-source applications and personal projects that run locally. Eligible Plus and Pro accounts can authorize plan usage; availability and model access still depend on the account. Paid or remotely hosted applications need the appropriate OpenAI access before distribution. Sources checked October 29, 2026: [official integration guide](https://developers.openai.com/cookbook/articles/sign-in-with-chatgpt) and [plan usage documentation](https://developers.openai.com/siwc/token-sharing-open-source).

## Connect

Run `picos login --direct` and complete the browser authorization. A successful sign-in gives picos permission to request inference using the selected ChatGPT plan. Your existing Codex login remains separate. Manage the application's plan allowance or revoke access in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).

The implementation currently keeps one account registration per picos home directory. It rejects a returning identity that differs from the saved account. Account switching and an in-app sign-out/revocation command are not implemented. Authentication storage currently requires Unix ownership and private-file permissions.

## Protocol and storage

`src/direct_auth.rs` implements the documented public-client OAuth flow: a `127.0.0.1` loopback callback, fresh state and nonce, PKCE S256, a persistent host UUID, and the issued client ID returned during first registration. The initial request uses `dynamic_agent_client`; later requests reuse the issued registration. The code exchange and refresh use the public OAuth token endpoint. Identity validation checks the JWKS signature, issuer, audience, expiry, authorized party, subject, and login nonce. A usable grant must include `chatgpt.tokens.use.direct` and `resource.invoke`. See [registration and sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in).

Authentication files live under `<picos home>/auth`: `host.json`, `registration.json`, and `session.json`. The directory is owner-only, credential files are `0600`, and updates use an atomic rename. Symlinks and unsafe permissions are rejected. A process lock serializes rotating-token refreshes. Terminal refresh errors clear only unusable tokens while retaining the issued registration and host ID; transient errors preserve credentials. See [accounts and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions) and [error recovery](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery).

Credentials are handled by the trusted host broker. Neither VM tools nor observer APIs receive them. HTTP clients require HTTPS, disable redirects and environment proxies, and use fixed OpenAI endpoints. Authorization links contain no retained ID-token hint; token bodies and provider error descriptions are never printed. The loopback listener exists only during login.

## Inference and the experiment boundary

The direct provider uses the public Responses API with OAuth bearer authentication and only the picos tool definitions. It maintains its own conversation and turn limits. Tool requests remain untrusted until the Rust broker validates and dispatches them. Model-provided source is interpreted by the guest VM; it is never passed to a host shell, JavaScript engine, compiler, or generic code-execution tool. See [models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference) and [supported tools and limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

The host must contact OpenAI for model inference and authentication. Those fixed broker connections are infrastructure, not a networking capability available to guest programs. The model's inference computation and the host broker itself are outside the guest CPU/RAM/storage budget; guest execution and guest-created helpers are charged to the VM's limits.

## Verification

`cargo test --offline direct_auth --lib` exercises OAuth parameter construction, callback forgery and cancellation, incomplete grants, signed JWT verification and invalid claims, private atomic storage, and terminal refresh recovery. It uses generated fixture keys and temporary files, without a real account or network requests.

Passing those tests verifies the local protocol checks. Sign-in success verifies authorization; model discovery verifies visibility; only a completed live inference request verifies that this account's plan can power that request. These are distinct checks, and local tests do not establish live account eligibility or model availability.
