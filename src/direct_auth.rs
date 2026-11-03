//! Direct Sign in with ChatGPT for the VM-only harness.
//! Credentials belong to this broker, never to a guest or a model-visible tool.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{JwkSet, KeyOperations, PublicKeyUse},
};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Header, Method, Response, Server};
use uuid::Uuid;

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const DISCOVERY: &str = "https://auth.openai.com/.well-known/openid-configuration";
const RESOURCE: &str = "https://api.openai.com/v1";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const MAX_AUTH_BYTES: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Host {
    id: String,
}
#[derive(Serialize, Deserialize)]
struct Registration {
    client_id: String,
}
#[derive(Serialize, Deserialize)]
struct Credentials {
    issuer: String,
    subject: String,
    email: Option<String>,
    client_id: String,
    access_token: String,
    refresh_token: String,
    id_token: String,
    scopes: Vec<String>,
    expires_at: u64,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: String,
}
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    id_token_signing_alg_values_supported: Vec<String>,
}
#[derive(Clone, Deserialize)]
struct Identity {
    sub: String,
    aud: serde_json::Value,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
}
struct Pending {
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
    client_id: Option<String>,
}
struct Callback {
    code: String,
    client_id: String,
}
#[derive(Debug)]
struct CallbackError {
    message: &'static str,
    state_verified: bool,
}
impl CallbackError {
    fn untrusted(message: &'static str) -> Self {
        Self {
            message,
            state_verified: false,
        }
    }
    fn verified(message: &'static str) -> Self {
        Self {
            message,
            state_verified: true,
        }
    }
}
struct TokenError {
    message: String,
    terminal_refresh: bool,
}
impl From<String> for TokenError {
    fn from(message: String) -> Self {
        Self {
            message,
            terminal_refresh: false,
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn secret_random() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn client() -> Result<Client, String> {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|_| "Could not initialize the ChatGPT HTTPS connection.".into())
}
fn auth_dir() -> Result<PathBuf, String> {
    Ok(crate::cli::picos_home()?.join("auth"))
}
fn check_private(path: &Path, directory: bool) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "Cannot read picos authentication storage.")?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(
            "Authentication storage must use ordinary files and directories, not symlinks.".into(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("Authentication storage must be owned by you and private (directory 0700, files 0600).".into());
        }
    }
    #[cfg(not(unix))]
    return Err(
        "Direct ChatGPT authentication currently requires Unix private-file permissions.".into(),
    );
    Ok(())
}
fn ensure_dir(path: &Path) -> Result<(), String> {
    if !path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .map_err(|_| "Could not create private picos authentication storage.")?;
        }
        #[cfg(not(unix))]
        return Err(
            "Direct ChatGPT authentication currently requires Unix private-file permissions."
                .into(),
        );
    }
    check_private(path, true)
}
fn open_private(path: &Path, create_new: bool) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|_| "Could not open private picos authentication storage.".into())
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    if !path
        .try_exists()
        .map_err(|_| "Cannot inspect picos authentication storage.")?
    {
        return Ok(None);
    }
    check_private(path, false)?;
    let mut bytes = Vec::new();
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|_| "Cannot open picos authentication storage.")?
        .take(MAX_AUTH_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read picos authentication storage.")?;
    if bytes.len() as u64 > MAX_AUTH_BYTES {
        return Err("Authentication record is too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "Invalid picos authentication record. Sign in again.".into())
}
fn save_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if path.exists() {
        check_private(path, false)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = open_private(&temporary, true)?;
        serde_json::to_writer(&mut file, value)
            .map_err(|_| "Could not encode authentication record.")?;
        file.write_all(b"\n")
            .map_err(|_| "Could not write authentication record.")?;
        file.sync_all()
            .map_err(|_| "Could not synchronize authentication record.")?;
        fs::rename(&temporary, path).map_err(|_| "Could not replace authentication record.")?;
        File::open(path.parent().unwrap())
            .and_then(|dir| dir.sync_all())
            .map_err(|_| "Could not synchronize authentication directory.")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
struct AuthLock(File);
impl AuthLock {
    fn acquire(dir: &Path) -> Result<Self, String> {
        ensure_dir(dir)?;
        let path = dir.join(".lock");
        let file = open_private(&path, false)?;
        check_private(&path, false)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let started = Instant::now();
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Ok(Self(file));
                }
                if started.elapsed() >= Duration::from_secs(10) {
                    return Err("Another picos process is updating ChatGPT authentication. Try again after it finishes.".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        #[cfg(not(unix))]
        Err("Direct ChatGPT authentication requires a supported private credential store.".into())
    }
}
impl Drop for AuthLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}
fn check_credentials(credentials: &Credentials) -> Result<(), String> {
    if credentials.issuer != ISSUER
        || credentials.subject.is_empty()
        || !valid_client_id(&credentials.client_id)
        || credentials.access_token.is_empty()
        || credentials.refresh_token.is_empty()
        || !credentials.scopes.iter().any(|s| s == PLAN_SCOPE)
        || !credentials.scopes.iter().any(|s| s == "resource.invoke")
    {
        return Err("No valid picos ChatGPT plan grant. Run picos login --direct.".into());
    }
    Ok(())
}
fn credentials(dir: &Path) -> Result<Credentials, String> {
    let credentials = read_json::<Credentials>(&dir.join("session.json"))?
        .ok_or("Strict mode needs its own ChatGPT plan authorization. Run picos login --direct; Codex credentials are not reused.")?;
    check_credentials(&credentials)?;
    Ok(credentials)
}
fn valid_client_id(value: &str) -> bool {
    !value.is_empty()
        && value != "dynamic_agent_client"
        && value.len() <= 256
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
fn host_id(dir: &Path) -> Result<String, String> {
    let file = dir.join("host.json");
    if let Some(host) = read_json::<Host>(&file)? {
        if host
            .id
            .strip_prefix("urn:uuid:")
            .is_none_or(|v| Uuid::parse_str(v).is_err())
        {
            return Err("Invalid picos authentication host identifier.".into());
        }
        return Ok(host.id);
    }
    let host = Host {
        id: format!("urn:uuid:{}", Uuid::new_v4()),
    };
    save_json(&file, &host)?;
    Ok(host.id)
}
fn authorization_url(pending: &Pending, host: &str) -> Url {
    let mut url = Url::parse(AUTHORIZE).unwrap();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));
    let mut query = url.query_pairs_mut();
    query
        .append_pair(
            "client_id",
            pending
                .client_id
                .as_deref()
                .unwrap_or("dynamic_agent_client"),
        )
        .append_pair("ext_agent_host_id", host)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &pending.redirect)
        .append_pair("scope", SCOPES)
        .append_pair("resource", RESOURCE)
        .append_pair("state", &pending.state)
        .append_pair("nonce", &pending.nonce)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge);
    if pending.client_id.is_none() {
        query.append_pair("agent_name_hint", "picos");
    }
    drop(query);
    url
}
fn parse_callback(request_url: &str, pending: &Pending) -> Result<Callback, CallbackError> {
    if request_url.len() > 16384 {
        return Err(CallbackError::untrusted("Invalid sign-in callback."));
    }
    let url = Url::parse(&format!("http://127.0.0.1{request_url}"))
        .map_err(|_| CallbackError::untrusted("Invalid sign-in callback."))?;
    if url.path() != "/auth/callback" {
        return Err(CallbackError::untrusted("Unknown sign-in callback path."));
    }
    let mut params = HashMap::new();
    for (key, value) in url.query_pairs() {
        if params
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(CallbackError::untrusted(
                "Duplicate sign-in callback parameter.",
            ));
        }
    }
    if params.get("state") != Some(&pending.state) {
        return Err(CallbackError::untrusted(
            "Sign-in state did not match this attempt.",
        ));
    }
    if params.contains_key("error") {
        return Err(CallbackError::verified(
            "ChatGPT authorization was declined or could not be completed.",
        ));
    }
    let client_id = match (&pending.client_id, params.get("client_id")) {
        (Some(expected), Some(actual)) if expected != actual => {
            return Err(CallbackError::verified(
                "Sign-in returned a different client registration.",
            ));
        }
        (Some(expected), _) => expected.clone(),
        (None, Some(actual)) if valid_client_id(actual) => actual.clone(),
        _ => {
            return Err(CallbackError::verified(
                "Sign-in did not return an issued client registration.",
            ));
        }
    };
    let code = params
        .remove("code")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| CallbackError::verified("Sign-in did not return an authorization code."))?;
    Ok(Callback { code, client_id })
}
fn read_response<T: DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T, String> {
    if !response.status().is_success() {
        // Provider error bodies may contain token material. Never return or log them.
        return Err(format!(
            "ChatGPT authorization request failed (HTTP {}). No billing fallback was attempted.",
            response.status().as_u16()
        ));
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_AUTH_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "ChatGPT authorization response was interrupted.")?;
    if bytes.len() as u64 > MAX_AUTH_BYTES {
        return Err("ChatGPT authorization response is too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "ChatGPT returned an invalid authorization response.".into())
}
fn token_failure(status: u16, body: &[u8]) -> TokenError {
    // Interpret only documented machine codes. Never echo provider descriptions or bodies.
    let value = serde_json::from_slice::<serde_json::Value>(body).ok();
    let code = value
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|v| v.as_str());
    let terminal_refresh = (400..500).contains(&status)
        && matches!(
            code,
            Some(
                "invalid_grant"
                    | "invalid_refresh_token"
                    | "token_expired"
                    | "refresh_token_expired"
                    | "refresh_token_invalidated"
                    | "refresh_token_reused"
            )
        );
    TokenError {
        terminal_refresh,
        message: format!(
            "ChatGPT authorization request failed (HTTP {status}). No billing fallback was attempted."
        ),
    }
}
fn token_request(client: &Client, fields: &[(&str, &str)]) -> Result<TokenResponse, TokenError> {
    let response = client.post(TOKEN).form(fields).send().map_err(|_| {
        TokenError::from(
            "Could not reach ChatGPT authorization. Saved credentials were preserved.".to_string(),
        )
    })?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let mut body = Vec::new();
        if response
            .take(MAX_AUTH_BYTES + 1)
            .read_to_end(&mut body)
            .is_err()
            || body.len() as u64 > MAX_AUTH_BYTES
        {
            body.clear();
        }
        return Err(token_failure(status, &body));
    }
    read_response(response).map_err(TokenError::from)
}
fn refresh_result(
    dir: &Path,
    response: Result<TokenResponse, TokenError>,
) -> Result<TokenResponse, String> {
    response.map_err(|error| {
        if error.terminal_refresh {
            // Keep the issued client and host registration; only the unusable token bundle goes.
            if fs::remove_file(dir.join("session.json")).is_err() {
                return "ChatGPT authorization expired, but its local token record could not be removed. Run picos login --direct.".into();
            }
            "ChatGPT authorization expired or was revoked. Run picos login --direct to reconnect using the saved registration.".into()
        } else {
            error.message
        }
    })
}
fn validate_identity(
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    discovery: &Discovery,
    jwks: &JwkSet,
) -> Result<Identity, String> {
    if discovery.issuer != ISSUER {
        return Err("Unexpected ChatGPT identity issuer.".into());
    }
    let header = decode_header(token).map_err(|_| "Invalid ChatGPT identity token.")?;
    if matches!(
        header.alg,
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
    ) || !discovery
        .id_token_signing_alg_values_supported
        .contains(&format!("{:?}", header.alg))
    {
        return Err("Unsupported ChatGPT identity signature algorithm.".into());
    }
    let key = header
        .kid
        .as_deref()
        .and_then(|kid| jwks.find(kid))
        .ok_or("ChatGPT identity signing key was not found.")?;
    if key
        .common
        .key_algorithm
        .is_some_and(|alg| format!("{alg:?}") != format!("{:?}", header.alg))
    {
        return Err("ChatGPT identity signing algorithm did not match its key.".into());
    }
    if key
        .common
        .public_key_use
        .as_ref()
        .is_some_and(|use_| use_ != &PublicKeyUse::Signature)
        || key
            .common
            .key_operations
            .as_ref()
            .is_some_and(|ops| !ops.contains(&KeyOperations::Verify))
    {
        return Err("ChatGPT identity key is not authorized for signature verification.".into());
    }
    let key = DecodingKey::from_jwk(key).map_err(|_| "Invalid ChatGPT identity signing key.")?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 0;
    validation.validate_nbf = true;
    let identity = decode::<Identity>(token, &key, &validation)
        .map_err(|_| "ChatGPT identity signature, issuer, audience, or expiry validation failed.")?
        .claims;
    if identity.sub.is_empty()
        || nonce.is_some_and(|expected| identity.nonce.as_deref() != Some(expected))
    {
        return Err("ChatGPT identity nonce or subject validation failed.".into());
    }
    if identity
        .azp
        .as_deref()
        .is_some_and(|party| party != client_id)
        || (identity.aud.as_array().is_some_and(|aud| aud.len() > 1)
            && identity.azp.as_deref() != Some(client_id))
    {
        return Err("ChatGPT identity authorized-party validation failed.".into());
    }
    Ok(identity)
}
fn identity_from_network(
    client: &Client,
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
) -> Result<Identity, String> {
    let discovery: Discovery = read_response(
        client
            .get(DISCOVERY)
            .send()
            .map_err(|_| "Could not fetch ChatGPT identity configuration.")?,
    )?;
    let uri =
        Url::parse(&discovery.jwks_uri).map_err(|_| "Invalid ChatGPT signing-key address.")?;
    if discovery.issuer != ISSUER
        || uri.scheme() != "https"
        || uri.host_str() != Some("auth.openai.com")
        || uri.port_or_known_default() != Some(443)
        || !uri.username().is_empty()
        || uri.password().is_some()
    {
        return Err("Unexpected ChatGPT signing-key origin.".into());
    }
    let keys = read_response(
        client
            .get(uri)
            .send()
            .map_err(|_| "Could not fetch ChatGPT signing keys.")?,
    )?;
    validate_identity(token, client_id, nonce, &discovery, &keys)
}
fn validate_grant(response: &TokenResponse) -> Result<Vec<String>, String> {
    let scopes: Vec<String> = response
        .scope
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if !response.token_type.eq_ignore_ascii_case("Bearer")
        || response.access_token.is_empty()
        || response.refresh_token.as_ref().is_none_or(String::is_empty)
        || response.expires_in == 0
        || response.expires_in > 86400
        || ![PLAN_SCOPE, "resource.invoke"]
            .iter()
            .all(|expected| scopes.iter().any(|s| s == expected))
    {
        return Err("ChatGPT did not grant renewable plan usage to picos. Run picos login --direct and authorize ChatGPT plan usage.".into());
    }
    Ok(scopes)
}
fn browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let result = Command::new("xdg-open").arg(url).spawn();
    let mut child = result
        .map_err(|_| "Could not open the browser. Open the printed authorization link manually.")?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Obtain a picos-specific SIWC grant. This never reads or changes Codex login.
pub fn login(open_browser: bool) -> Result<(), String> {
    let dir = auth_dir()?;
    let _lock = AuthLock::acquire(&dir)?;
    let host = host_id(&dir)?;
    let previous = read_json::<Credentials>(&dir.join("session.json"))?;
    let registration = read_json::<Registration>(&dir.join("registration.json"))?;
    let client_id = previous
        .as_ref()
        .map(|c| c.client_id.clone())
        .or(registration.map(|r| r.client_id));
    if client_id.as_ref().is_some_and(|v| !valid_client_id(v)) {
        return Err("Invalid saved ChatGPT registration.".into());
    }
    let server = Server::http(("127.0.0.1", 0))
        .map_err(|_| "Could not start the local ChatGPT sign-in callback.")?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or("Missing local callback address.")?
        .port();
    let pending = Pending {
        state: secret_random(),
        nonce: secret_random(),
        verifier: secret_random(),
        redirect: format!("http://127.0.0.1:{port}/auth/callback"),
        client_id,
    };
    let url = authorization_url(&pending, &host);
    println!(
        "Continue with ChatGPT to authorize picos to use your plan. Codex sign-in is separate.\n{url}"
    );
    if open_browser {
        if let Err(message) = browser(url.as_str()) {
            eprintln!("{message}");
        }
    }
    let started = Instant::now();
    let callback = loop {
        if started.elapsed() >= Duration::from_secs(600) {
            return Err("ChatGPT sign-in timed out. Run picos login --direct again.".into());
        }
        let Some(request) = server
            .recv_timeout(Duration::from_millis(500))
            .map_err(|_| "ChatGPT callback listener failed.")?
        else {
            continue;
        };
        let valid_origin = request.remote_addr().is_some_and(|v| v.ip().is_loopback())
            && request.method() == &Method::Get
            && request
                .headers()
                .iter()
                .any(|h| h.field.equiv("Host") && h.value.as_str() == format!("127.0.0.1:{port}"));
        let result = if valid_origin {
            parse_callback(request.url(), &pending)
        } else {
            Err(CallbackError::untrusted("Invalid local callback request."))
        };
        let text = if result.is_ok() {
            "ChatGPT authorization received. Return to your terminal while picos validates it."
        } else {
            "This sign-in callback was not accepted. Return to your terminal."
        };
        let mut reply =
            Response::from_string(text).with_status_code(if result.is_ok() { 200 } else { 400 });
        reply.add_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
        let _ = request.respond(reply);
        // A malformed unrelated local request must not cancel a real pending login.
        match result {
            Ok(callback) => break callback,
            Err(error) if error.state_verified => return Err(error.message.into()),
            Err(_) => continue,
        }
    };
    save_json(
        &dir.join("registration.json"),
        &Registration {
            client_id: callback.client_id.clone(),
        },
    )?;
    let client = client()?;
    let response = token_request(
        &client,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &callback.client_id),
            ("code", &callback.code),
            ("code_verifier", &pending.verifier),
            ("redirect_uri", &pending.redirect),
            ("resource", RESOURCE),
        ],
    )
    .map_err(|error| error.message)?;
    let scopes = validate_grant(&response)?;
    let id_token = response
        .id_token
        .ok_or("ChatGPT did not return an identity token.")?;
    let identity = identity_from_network(
        &client,
        &id_token,
        &callback.client_id,
        Some(&pending.nonce),
    )?;
    if previous
        .as_ref()
        .is_some_and(|p| p.subject != identity.sub || p.client_id != callback.client_id)
    {
        return Err("The signed-in account differs from the saved picos account. Existing credentials were preserved.".into());
    }
    let saved = Credentials {
        issuer: ISSUER.into(),
        subject: identity.sub,
        email: identity.email,
        client_id: callback.client_id,
        access_token: response.access_token,
        refresh_token: response.refresh_token.unwrap(),
        id_token,
        scopes,
        expires_at: now().saturating_add(response.expires_in),
    };
    save_json(&dir.join("session.json"), &saved)?;
    println!(
        "picos is connected using your ChatGPT plan. Manage app usage at https://chatgpt.com/settings/usage"
    );
    Ok(())
}
/// Safe display status. A connected grant is not proof of completed inference.
pub fn status() -> Result<String, String> {
    let dir = auth_dir()?;
    if !dir.exists() {
        return Ok("Direct ChatGPT: not connected. Run picos login --direct.".into());
    }
    let _lock = AuthLock::acquire(&dir)?;
    match read_json::<Credentials>(&dir.join("session.json"))? {
        Some(credentials) => {
            check_credentials(&credentials)?;
            Ok(format!(
                "Direct ChatGPT: connected{}. ChatGPT plan usage authorized; {}. Model access is verified when an inference request completes.",
                credentials
                    .email
                    .as_ref()
                    .map(|v| format!(" as {}", v.replace(['\r', '\n'], "")))
                    .unwrap_or_default(),
                if credentials.expires_at <= now() + 60 {
                    "token renewal needed"
                } else {
                    "access token current"
                }
            ))
        }
        None => Ok("Direct ChatGPT: not connected. Run picos login --direct.".into()),
    }
}
pub fn preflight() -> Result<(), String> {
    let dir = auth_dir()?;
    if !dir.exists() {
        return Err(
            "Strict mode requires picos ChatGPT authorization. Run picos login --direct.".into(),
        );
    }
    let _lock = AuthLock::acquire(&dir)?;
    credentials(&dir).map(|_| ())
}
/// Returns only to the trusted HTTP broker. Never expose this through a tool.
pub fn access_token() -> Result<String, String> {
    let dir = auth_dir()?;
    let _lock = AuthLock::acquire(&dir)?;
    let mut saved = credentials(&dir)?;
    if saved.expires_at > now().saturating_add(60) {
        return Ok(saved.access_token);
    }
    let client = client()?;
    let response = refresh_result(
        &dir,
        token_request(
            &client,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &saved.client_id),
                ("refresh_token", &saved.refresh_token),
                ("resource", RESOURCE),
            ],
        ),
    )?;
    let scopes = validate_grant(&response)?;
    if let Some(id_token) = response.id_token {
        let identity = identity_from_network(&client, &id_token, &saved.client_id, None)?;
        if identity.sub != saved.subject {
            return Err(
                "ChatGPT renewal returned a different identity. Saved credentials were preserved."
                    .into(),
            );
        }
        saved.id_token = id_token;
        saved.email = identity.email.or(saved.email);
    }
    saved.access_token = response.access_token;
    saved.refresh_token = response.refresh_token.unwrap();
    saved.expires_at = now().saturating_add(response.expires_in);
    saved.scopes = scopes;
    save_json(&dir.join("session.json"), &saved)?;
    Ok(saved.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pending() -> Pending {
        Pending {
            state: "correct-state".into(),
            nonce: "correct-nonce".into(),
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
            redirect: "http://127.0.0.1:12345/auth/callback".into(),
            client_id: None,
        }
    }
    #[test]
    fn authorization_uses_documented_public_client_pkce_and_plan_scope() {
        let url = authorization_url(&pending(), "urn:uuid:00000000-0000-4000-8000-000000000000");
        let params: HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(url.origin().ascii_serialization(), ISSUER);
        assert_eq!(params["client_id"], "dynamic_agent_client");
        assert_eq!(
            params["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(params["scope"], SCOPES);
        assert_eq!(params["resource"], RESOURCE);
        assert!(!params.contains_key("client_secret"));
        assert!(!params.contains_key("id_token_hint"));
        let mut p = pending();
        p.client_id = Some("oaiapp_saved".into());
        let returning = authorization_url(&p, "host");
        assert!(
            !returning
                .query_pairs()
                .any(|(key, _)| key == "agent_name_hint")
        );
    }
    #[test]
    fn callbacks_reject_forgery_duplicates_declines_and_registration_changes() {
        let p = pending();
        let ok = parse_callback(
            "/auth/callback?state=correct-state&code=code&client_id=oaiapp_new",
            &p,
        )
        .unwrap();
        assert_eq!(ok.client_id, "oaiapp_new");
        assert!(
            parse_callback("/auth/callback?state=correct-state&error=access_denied", &p)
                .err()
                .unwrap()
                .state_verified
        );
        assert!(
            !parse_callback("/auth/callback?state=wrong&error=access_denied", &p)
                .err()
                .unwrap()
                .state_verified
        );
        for url in [
            "/auth/callback?state=wrong&code=code&client_id=oaiapp_new",
            "/auth/callback?state=correct-state&state=correct-state&code=code&client_id=oaiapp_new",
            "/auth/callback?state=correct-state&error=access_denied&code=code&client_id=oaiapp_new",
            "/auth/callback?state=correct-state&code=code&client_id=dynamic_agent_client",
            "/auth/callback?state=correct-state&code=code",
            "/elsewhere?state=correct-state&code=code&client_id=oaiapp_new",
        ] {
            assert!(parse_callback(url, &p).is_err(), "accepted {url}");
        }
        let mut p = pending();
        p.client_id = Some("oaiapp_saved".into());
        assert!(
            parse_callback(
                "/auth/callback?state=correct-state&code=code&client_id=oaiapp_other",
                &p
            )
            .is_err()
        );
        assert_eq!(
            parse_callback("/auth/callback?state=correct-state&code=code", &p)
                .unwrap()
                .client_id,
            "oaiapp_saved"
        );
    }
    #[test]
    fn identity_only_and_nonrenewable_grants_are_rejected() {
        let value = json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","id_token":"fixture-id","token_type":"Bearer","expires_in":3600,"scope":SCOPES});
        let valid: TokenResponse = serde_json::from_value(value.clone()).unwrap();
        assert!(validate_grant(&valid).is_ok());
        for (field, bad) in [
            ("scope", json!("openid profile email")),
            ("refresh_token", json!(null)),
            ("access_token", json!("")),
            ("token_type", json!("other")),
            ("expires_in", json!(0)),
        ] {
            let mut changed = value.clone();
            changed[field] = bad;
            assert!(validate_grant(&serde_json::from_value(changed).unwrap()).is_err());
        }
    }
    #[test]
    fn jwt_rejects_symmetric_algorithms_before_considering_claims() {
        use jsonwebtoken::{EncodingKey, Header, encode};
        let token = encode(
            &Header::new(Algorithm::HS256),
            &json!({"sub":"someone","aud":"oaiapp_fixture","iss":ISSUER,"exp":now()+3600}),
            &EncodingKey::from_secret(b"fixture"),
        )
        .unwrap();
        let discovery = Discovery {
            issuer: ISSUER.into(),
            jwks_uri: "https://auth.openai.com/keys".into(),
            id_token_signing_alg_values_supported: vec!["HS256".into()],
        };
        assert!(
            validate_identity(
                &token,
                "oaiapp_fixture",
                Some("nonce"),
                &discovery,
                &JwkSet { keys: vec![] }
            )
            .is_err()
        );
    }
    #[test]
    fn signed_identity_requires_authentic_signature_and_bound_oidc_claims() {
        use jsonwebtoken::{EncodingKey, Header, encode};
        // A generated, public test fixture. It is never used for live authentication.
        let fixture_key = URL_SAFE_NO_PAD
            .decode("MC4CAQAwBQYDK2VwBCIEIKOAe6BMBwoLFxuLOEhgBWC2fVI9jb2JFMunT7a7njQ_")
            .unwrap();
        let keys: JwkSet = serde_json::from_value(json!({"keys":[{
            "kty":"OKP", "crv":"Ed25519", "use":"sig", "key_ops":["verify"],
            "alg":"EdDSA", "kid":"fixture-key",
            "x":"byVhze2Kssa6QiP_K1hJPxOtTUe2Jvsamayj_0lngog"
        }]}))
        .unwrap();
        let discovery = Discovery {
            issuer: ISSUER.into(),
            jwks_uri: "https://auth.openai.com/keys".into(),
            id_token_signing_alg_values_supported: vec!["EdDSA".into()],
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some("fixture-key".into());
        let sign = |claims: &serde_json::Value| {
            encode(&header, claims, &EncodingKey::from_ed_der(&fixture_key)).unwrap()
        };
        let claims = json!({"sub":"fixture-subject", "aud":"oaiapp_fixture", "iss":ISSUER,
            "exp":now()+3600, "nonce":"correct-nonce"});
        let validate = |token: &str| {
            validate_identity(
                token,
                "oaiapp_fixture",
                Some("correct-nonce"),
                &discovery,
                &keys,
            )
        };
        let token = sign(&claims);
        assert!(validate(&token).is_ok());
        for (field, bad) in [
            ("sub", json!("")),
            ("aud", json!("another-client")),
            ("iss", json!("https://other.example")),
            ("exp", json!(now() - 120)),
            ("nonce", json!("wrong-nonce")),
            ("nonce", json!(null)),
            ("azp", json!("another-client")),
            ("aud", json!(["oaiapp_fixture", "other-client"])),
        ] {
            let mut changed = claims.clone();
            changed[field] = bad;
            assert!(
                validate(&sign(&changed)).is_err(),
                "accepted invalid {field}"
            );
        }
        let mut multiple_audience = claims.clone();
        multiple_audience["aud"] = json!(["oaiapp_fixture", "other-client"]);
        multiple_audience["azp"] = json!("oaiapp_fixture");
        assert!(validate(&sign(&multiple_audience)).is_ok());
        let mut segments: Vec<String> = token.split('.').map(str::to_owned).collect();
        let mut forged_claims = claims;
        forged_claims["sub"] = json!("forged-subject");
        segments[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged_claims).unwrap());
        assert!(validate(&segments.join(".")).is_err());
        let mut wrong_use = keys.clone();
        wrong_use.keys[0].common.public_key_use = Some(PublicKeyUse::Encryption);
        assert!(
            validate_identity(
                &token,
                "oaiapp_fixture",
                Some("correct-nonce"),
                &discovery,
                &wrong_use
            )
            .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn terminal_refresh_errors_remove_only_tokens_and_never_echo_provider_bodies() {
        let dir = std::env::temp_dir().join(format!("picos-refresh-test-{}", Uuid::new_v4()));
        ensure_dir(&dir).unwrap();
        save_json(
            &dir.join("registration.json"),
            &Registration {
                client_id: "oaiapp_fixture".into(),
            },
        )
        .unwrap();
        let first_host = host_id(&dir).unwrap();
        for code in [
            "invalid_grant",
            "invalid_refresh_token",
            "token_expired",
            "refresh_token_expired",
            "refresh_token_invalidated",
            "refresh_token_reused",
        ] {
            save_json(
                &dir.join("session.json"),
                &json!({"access_token":"fixture-secret"}),
            )
            .unwrap();
            let body =
                serde_json::to_vec(&json!({"error":code,"error_description":"fixture-secret"}))
                    .unwrap();
            let error = token_failure(400, &body);
            assert!(error.terminal_refresh);
            let message = refresh_result(&dir, Err(error)).err().unwrap();
            assert!(!message.contains("fixture-secret"));
            assert!(!dir.join("session.json").exists());
            assert!(dir.join("registration.json").exists());
            assert_eq!(host_id(&dir).unwrap(), first_host);
        }
        save_json(
            &dir.join("session.json"),
            &json!({"access_token":"fixture-secret"}),
        )
        .unwrap();
        for (status, body) in [
            (500, br#"{"error":"invalid_grant"}"#.as_slice()),
            (
                400,
                br#"{"error":"unknown","error_description":"fixture-secret"}"#.as_slice(),
            ),
            (400, b"fixture-secret".as_slice()),
        ] {
            let message = refresh_result(&dir, Err(token_failure(status, body)))
                .err()
                .unwrap();
            assert!(!message.contains("fixture-secret"));
            assert!(dir.join("session.json").exists());
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn storage_is_private_atomic_and_does_not_follow_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("picos-auth-test-{}", Uuid::new_v4()));
        ensure_dir(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let file = dir.join("record.json");
        save_json(&file, &json!({"value":"first"})).unwrap();
        save_json(&file, &json!({"value":"second"})).unwrap();
        assert_eq!(
            read_json::<serde_json::Value>(&file).unwrap().unwrap()["value"],
            "second"
        );
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = dir.join("link.json");
        symlink(&file, &link).unwrap();
        assert!(read_json::<serde_json::Value>(&link).is_err());
        assert!(save_json(&link, &json!({"value":"replace"})).is_err());
        let first_host = host_id(&dir).unwrap();
        assert_eq!(host_id(&dir).unwrap(), first_host);
        fs::remove_dir_all(dir).unwrap();
    }
}
