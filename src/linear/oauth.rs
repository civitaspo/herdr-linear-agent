//! Linear OAuth authorization-code flow with PKCE, as an app actor.
//!
//! The flow stops at an in-memory token bundle; `credentials` persists and
//! refreshes it. Browser, clock, callback-listener, entropy, and
//! token-transport boundaries are injectable so the contract can be tested
//! without opening a browser or contacting Linear.
//!
//! Ported from Nagi (`crates/nagi/src/linear/oauth.rs` at `ee7d657`); the
//! requested scope is `read,write,app:assignable`.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use httparse::{self, Status};
use serde::Deserialize;
use zeroize::{Zeroize, Zeroizing};

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oauth2::basic::BasicClient;
use oauth2::reqwest::blocking::{Client, ClientBuilder};
use oauth2::reqwest::redirect::Policy;
use oauth2::url::form_urlencoded::Serializer;
use oauth2::{
    AuthUrl, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenUrl,
};

const AUTHORIZATION_ENDPOINT: &str = "https://linear.app/oauth/authorize";
const TOKEN_ENDPOINT: &str = "https://api.linear.app/oauth/token";
const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PATH: &str = "/oauth/callback";
const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(180);
const REQUESTED_SCOPE: &str = "read,write,app:assignable";
const REQUESTED_ACTOR: &str = "app";

const STATE_RANDOM_BYTES: usize = 32;
const VERIFIER_RANDOM_BYTES: usize = 32;
const MAX_CALLBACK_REQUEST_BYTES: usize = 8 * 1024;
const MAX_CALLBACK_HEADERS: usize = 32;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CALLBACK_RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);

const CALLBACK_SUCCESS_BODY: &[u8] = b"Authorization complete. You may close this window.";
const CALLBACK_FAILURE_BODY: &[u8] = b"Authorization failed. You may close this window.";

/// Errors produced while validating local OAuth configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigurationError {
    /// The client identifier was empty.
    EmptyClientId,
    /// The client identifier contained a control character.
    InvalidClientId,
    /// The callback port was zero or otherwise invalid.
    InvalidCallbackPort,
    /// A fixed OAuth endpoint could not be parsed.
    InvalidFixedEndpoint,
}

impl fmt::Display for ConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyClientId => "Linear OAuth client configuration is incomplete",
            Self::InvalidClientId => "Linear OAuth client configuration is invalid",
            Self::InvalidCallbackPort => "Linear OAuth callback port is invalid",
            Self::InvalidFixedEndpoint => "Linear OAuth endpoint configuration is invalid",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ConfigurationError {}

/// Immutable local configuration for the Linear OAuth flow.
///
/// Only the client identifier and numeric loopback port are configurable.  The
/// callback host, scheme, path, authorization endpoint, token endpoint, actor,
/// and scope are fixed by this module.
#[derive(Clone, Eq, PartialEq)]
pub struct OAuthConfig {
    client_id: String,
    callback_port: u16,
}

impl OAuthConfig {
    /// Creates configuration using the production callback port.
    #[cfg(test)]
    pub fn new(client_id: impl Into<String>) -> Result<Self, ConfigurationError> {
        Self::with_callback_port(client_id, crate::config::DEFAULT_CALLBACK_PORT)
    }

    /// Creates configuration with a numeric loopback callback port.
    ///
    /// Port zero is intentionally rejected.  Tests that need an ephemeral port
    /// can inject a callback listener rather than changing production config.
    pub fn with_callback_port(
        client_id: impl Into<String>,
        callback_port: u16,
    ) -> Result<Self, ConfigurationError> {
        let client_id = client_id.into();
        if client_id.is_empty() {
            return Err(ConfigurationError::EmptyClientId);
        }
        if client_id.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(ConfigurationError::InvalidClientId);
        }
        if callback_port == 0 {
            return Err(ConfigurationError::InvalidCallbackPort);
        }
        Ok(Self {
            client_id,
            callback_port,
        })
    }

    /// Returns the configured numeric callback port.
    #[cfg(test)]
    pub fn callback_port(&self) -> u16 {
        self.callback_port
    }

    /// Returns the fixed loopback callback URI.
    pub fn redirect_uri(&self) -> String {
        callback_uri(self.callback_port)
    }
}

impl fmt::Debug for OAuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthConfig")
            .field("client_id", &"[redacted]")
            .field("callback_port", &self.callback_port)
            .finish()
    }
}

/// Entropy failures are intentionally coarse and contain no provider or machine details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RandomError {
    /// The operating system could not provide random bytes.
    Unavailable,
}

impl fmt::Display for RandomError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("secure random generation failed")
    }
}

impl std::error::Error for RandomError {}

/// Source of cryptographically secure random bytes.
trait RandomSource {
    /// Fills `destination` with cryptographically secure random bytes.
    fn fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), RandomError>;
}

/// Operating-system random source used by production authorization.
#[derive(Debug, Default)]
struct OsRandom;

impl RandomSource for OsRandom {
    fn fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), RandomError> {
        getrandom::fill(destination).map_err(|_| RandomError::Unavailable)
    }
}

/// Monotonic clock used by production authorization.
#[derive(Debug, Default)]
struct MonotonicClock;

/// Clock abstraction used to derive and enforce the authorization deadline.
trait Clock {
    /// Returns the current monotonic instant.
    fn now(&self) -> Instant;
}

impl Clock for MonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Browser-launch failures are intentionally coarse and do not contain the URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserError {
    /// Browser launching is unsupported on this host.
    #[cfg_attr(
        all(any(target_os = "macos", target_os = "linux"), not(test)),
        expect(dead_code, reason = "constructed only on unsupported hosts")
    )]
    Unsupported,
    /// The direct browser-launch command failed.
    LaunchFailed,
}

impl fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unsupported => "browser launch is unsupported on this host",
            Self::LaunchFailed => "browser launch failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for BrowserError {}

/// Boundary for opening an authorization URL.
trait BrowserLauncher {
    /// Opens the supplied authorization URL before the authorization deadline.
    fn launch(
        &self,
        authorization_url: &str,
        deadline: Instant,
        clock: &dyn Clock,
    ) -> Result<(), BrowserError>;
}

/// Direct browser launcher: `/usr/bin/open` on macOS, `xdg-open` on Linux.
/// No shell is involved and the URL is never logged by this implementation.
#[derive(Debug, Default)]
struct SystemBrowserLauncher;

impl BrowserLauncher for SystemBrowserLauncher {
    fn launch(
        &self,
        authorization_url: &str,
        deadline: Instant,
        clock: &dyn Clock,
    ) -> Result<(), BrowserError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            if clock.now() >= deadline {
                return Err(BrowserError::LaunchFailed);
            }
            let mut child = Command::new(crate::files::OPEN_COMMAND)
                .arg(authorization_url)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| BrowserError::LaunchFailed)?;
            wait_for_browser_child(&mut child, deadline, clock, std::thread::sleep)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (authorization_url, deadline, clock);
            Err(BrowserError::Unsupported)
        }
    }
}

// Process seam is compiled for production hosts and unit tests.
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
trait ChildProcess {
    fn try_wait(&mut self) -> io::Result<Option<bool>>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<bool>;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ChildProcess for std::process::Child {
    fn try_wait(&mut self) -> io::Result<Option<bool>> {
        std::process::Child::try_wait(self).map(|status| status.map(|status| status.success()))
    }

    fn kill(&mut self) -> io::Result<()> {
        std::process::Child::kill(self)
    }

    fn wait(&mut self) -> io::Result<bool> {
        std::process::Child::wait(self).map(|status| status.success())
    }
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn wait_for_browser_child(
    child: &mut dyn ChildProcess,
    deadline: Instant,
    clock: &dyn Clock,
    sleep: impl Fn(Duration),
) -> Result<(), BrowserError> {
    loop {
        if clock.now() >= deadline {
            terminate_and_reap(child);
            return Err(BrowserError::LaunchFailed);
        }
        match child.try_wait() {
            Ok(Some(success)) => {
                // A process observable at or after the deadline is too late.
                if clock.now() >= deadline {
                    return Err(BrowserError::LaunchFailed);
                }
                return success.then_some(()).ok_or(BrowserError::LaunchFailed);
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(clock.now());
                if remaining.is_zero() {
                    terminate_and_reap(child);
                    return Err(BrowserError::LaunchFailed);
                }
                sleep(remaining.min(CALLBACK_POLL_INTERVAL));
            }
            Err(_) => {
                terminate_and_reap(child);
                return Err(BrowserError::LaunchFailed);
            }
        }
    }
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn terminate_and_reap(child: &mut dyn ChildProcess) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Errors while binding or accepting a loopback callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListenerError {
    /// The requested callback URI was not the fixed loopback URI shape.
    InvalidRedirectUri,
    /// The loopback listener could not be created.
    BindFailed,
}

impl fmt::Display for ListenerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidRedirectUri => "Linear OAuth callback URI is invalid",
            Self::BindFailed => "Linear OAuth callback listener could not bind",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ListenerError {}

/// Errors while parsing or validating a callback request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackError {
    /// The callback deadline elapsed before a complete request arrived.
    Timeout,
    /// The listener has already accepted one connection.
    AlreadyConsumed,
    /// The request exceeded the bounded parser input limit.
    OversizedRequest,
    /// The HTTP request was syntactically invalid.
    MalformedRequest,
    /// The request method was not GET.
    WrongMethod,
    /// The request target was not the exact callback origin-form path.
    WrongPath,
    /// The provider returned an OAuth error callback.
    ErrorCallback,
    /// A required callback parameter was missing.
    MissingParameter,
    /// A callback parameter appeared more than once.
    DuplicateParameter,
    /// A callback parameter could not be decoded or contained invalid characters.
    InvalidParameter,
    /// The callback state did not match the authorization state.
    StateMismatch,
    /// A callback connection failed at the local I/O boundary.
    IoFailure,
}

impl fmt::Display for CallbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Timeout => "Linear OAuth callback timed out",
            Self::AlreadyConsumed => "Linear OAuth callback was already consumed",
            Self::OversizedRequest => "Linear OAuth callback request is too large",
            Self::MalformedRequest => "Linear OAuth callback request is malformed",
            Self::WrongMethod => "Linear OAuth callback method is not allowed",
            Self::WrongPath => "Linear OAuth callback path is not allowed",
            Self::ErrorCallback => "Linear OAuth provider returned an authorization error",
            Self::MissingParameter => "Linear OAuth callback is incomplete",
            Self::DuplicateParameter => "Linear OAuth callback is ambiguous",
            Self::InvalidParameter => "Linear OAuth callback parameter is invalid",
            Self::StateMismatch => "Linear OAuth callback state did not match",
            Self::IoFailure => "Linear OAuth callback I/O failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CallbackError {}

/// The one-time callback values returned by a listener.
struct Callback {
    code: Secret,
    state: Secret,
}

impl Callback {
    /// Creates a callback value for a listener implementation.
    ///
    /// The values are retained only in zeroizing buffers and are never included
    /// in `Debug` output.
    #[cfg(test)]
    fn new(code: impl Into<String>, state: impl Into<String>) -> Result<Self, CallbackError> {
        let code = Secret::try_new(code.into()).ok_or(CallbackError::InvalidParameter)?;
        let state = Secret::try_new(state.into()).ok_or(CallbackError::InvalidParameter)?;
        Ok(Self { code, state })
    }
}

impl fmt::Debug for Callback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Callback([redacted])")
    }
}

/// Boundary for a one-time callback listener.
trait CallbackListener {
    /// Accepts and validates one callback request before the deadline.
    fn wait_for_callback(
        &mut self,
        deadline: Instant,
        clock: &dyn Clock,
        expected_state: &[u8],
    ) -> Result<Callback, CallbackError>;
}

/// Boundary for binding a callback listener before opening the browser.
trait CallbackListenerFactory {
    /// Binds a listener to the supplied fixed redirect URI.
    fn bind(&self, redirect_uri: &str) -> Result<Box<dyn CallbackListener>, ListenerError>;
}

/// Production callback-listener factory.
#[derive(Debug, Default)]
struct LoopbackCallbackListenerFactory;

impl CallbackListenerFactory for LoopbackCallbackListenerFactory {
    fn bind(&self, redirect_uri: &str) -> Result<Box<dyn CallbackListener>, ListenerError> {
        Ok(Box::new(LoopbackCallbackListener::bind(redirect_uri)?))
    }
}

/// One-time bounded listener on the local loopback interface.
struct LoopbackCallbackListener {
    listener: TcpListener,
    consumed: bool,
}

impl LoopbackCallbackListener {
    /// Binds a listener to the numeric port in a fixed loopback callback URI.
    fn bind(redirect_uri: &str) -> Result<Self, ListenerError> {
        let port = parse_callback_uri(redirect_uri).ok_or(ListenerError::InvalidRedirectUri)?;
        let listener =
            TcpListener::bind((CALLBACK_HOST, port)).map_err(|_| ListenerError::BindFailed)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ListenerError::BindFailed)?;
        Ok(Self {
            listener,
            consumed: false,
        })
    }

    /// Returns the local address selected by the operating system.
    #[cfg(test)]
    fn local_addr(&self) -> Result<std::net::SocketAddr, ListenerError> {
        self.listener
            .local_addr()
            .map_err(|_| ListenerError::BindFailed)
    }
}

impl fmt::Debug for LoopbackCallbackListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoopbackCallbackListener")
            .field("consumed", &self.consumed)
            .finish_non_exhaustive()
    }
}

impl CallbackListener for LoopbackCallbackListener {
    fn wait_for_callback(
        &mut self,
        deadline: Instant,
        clock: &dyn Clock,
        expected_state: &[u8],
    ) -> Result<Callback, CallbackError> {
        if self.consumed {
            return Err(CallbackError::AlreadyConsumed);
        }
        self.consumed = true;

        let (mut stream, _) = accept_until(&self.listener, deadline, clock)?;
        let result = match stream.set_nonblocking(false) {
            Ok(()) => read_callback(&mut stream, deadline, clock).and_then(|callback| {
                // Keep this concrete-listener check to choose the fixed
                // failure page; authorize repeats it for injected listeners.
                if clock.now() >= deadline {
                    Err(CallbackError::Timeout)
                } else if !constant_time_equal(callback.state.as_bytes(), expected_state) {
                    Err(CallbackError::StateMismatch)
                } else {
                    Ok(callback)
                }
            }),
            Err(_) => Err(CallbackError::IoFailure),
        };

        let success = result.is_ok();
        write_callback_response(&mut stream, success, deadline, clock);
        let _ = stream.shutdown(Shutdown::Both);
        result
    }
}

/// Response returned by a token transport.
///
/// The body is bounded and zeroized when this value is dropped.  It is parsed by
/// [`authorize`] and is never included in diagnostics.
struct TokenTransportResponse {
    status: u16,
    body: Zeroizing<Vec<u8>>,
}

impl TokenTransportResponse {
    /// Creates a bounded transport response.
    #[cfg(test)]
    fn new(status: u16, body: Vec<u8>) -> Result<Self, TokenTransportError> {
        if body.len() > MAX_TOKEN_RESPONSE_BYTES {
            let mut body = body;
            body.zeroize();
            return Err(TokenTransportError::ResponseTooLarge);
        }
        Ok(Self {
            status,
            body: Zeroizing::new(body),
        })
    }

    fn from_bounded_body(status: u16, body: Zeroizing<Vec<u8>>) -> Self {
        Self { status, body }
    }
}

impl fmt::Debug for TokenTransportResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenTransportResponse")
            .field("status", &self.status)
            .field("body", &"[redacted]")
            .finish()
    }
}

/// Token transport failures.  Provider payloads and network diagnostics are not retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenTransportError {
    /// The bounded HTTPS client could not be constructed.
    ClientConfiguration,
    /// The single token request failed before a response was received.
    RequestFailed,
    /// The response body exceeded the bounded parser limit.
    ResponseTooLarge,
}

impl fmt::Display for TokenTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ClientConfiguration => "Linear OAuth token transport is unavailable",
            Self::RequestFailed => "Linear OAuth token request failed",
            Self::ResponseTooLarge => "Linear OAuth token response is too large",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for TokenTransportError {}

/// Boundary for exchanging one authorization code for one token response.
trait TokenTransport {
    /// Performs exactly one token exchange for `request`.
    fn exchange(
        &mut self,
        request: &TokenExchangeRequest,
    ) -> Result<TokenTransportResponse, TokenTransportError>;
}

/// Fixed-endpoint HTTPS token transport used by production authorization.
///
/// This deliberately does not use oauth2's request adapter: that adapter returns
/// unbounded response buffers and its parse errors retain raw response bytes.
/// The local transport keeps the request and response bounded and maps every
/// network failure to a redacted coarse error instead.
struct HttpsTokenTransport {
    client: Client,
}

impl HttpsTokenTransport {
    /// Constructs a transport with redirects, proxies, and retries disabled.
    fn new() -> Result<Self, TokenTransportError> {
        let client = ClientBuilder::new()
            .https_only(true)
            .redirect(Policy::none())
            .no_proxy()
            .retry(oauth2::reqwest::retry::never())
            .connect_timeout(TOKEN_CONNECT_TIMEOUT)
            .timeout(TOKEN_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| TokenTransportError::ClientConfiguration)?;
        Ok(Self { client })
    }
}

impl fmt::Debug for HttpsTokenTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HttpsTokenTransport([configured])")
    }
}

impl TokenTransport for HttpsTokenTransport {
    fn exchange(
        &mut self,
        request: &TokenExchangeRequest,
    ) -> Result<TokenTransportResponse, TokenTransportError> {
        let body = request.form_body();
        let body_length = body.len() as u64;
        let response = self
            .client
            .post(TOKEN_ENDPOINT)
            .header(
                oauth2::reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(oauth2::reqwest::blocking::Body::sized(
                SecretBody::new(body),
                body_length,
            ))
            .send()
            .map_err(|_| TokenTransportError::RequestFailed)?;
        let status = response.status().as_u16();
        let body = read_bounded_response(response)?;
        Ok(TokenTransportResponse::from_bounded_body(status, body))
    }
}

/// Authorization request prepared after secure state and PKCE generation.
struct AuthorizationRequest {
    authorization_url: Zeroizing<String>,
    redirect_uri: String,
    state: Secret,
    verifier: Secret,
}

impl AuthorizationRequest {
    /// Returns the URL that should be passed directly to a browser launcher.
    fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// Returns the fixed loopback redirect URI.
    fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }
}

impl fmt::Debug for AuthorizationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizationRequest")
            .field("authorization_url", &"[redacted]")
            .field("redirect_uri", &self.redirect_uri)
            .field("state", &"[redacted]")
            .field("verifier", &"[redacted]")
            .finish()
    }
}

/// Prepares one authorization URL without opening a browser or listener.
fn prepare_authorization(
    config: &OAuthConfig,
    random: &mut dyn RandomSource,
) -> Result<AuthorizationRequest, OAuthError> {
    let redirect_uri = config.redirect_uri();

    // Use oauth2's typed URL, CSRF, and PKCE primitives.  The convenience random
    // constructors use a process-global RNG, so the equivalent 32-byte values
    // are supplied through the injectable source here and consumed into the
    // typed values below.  The generated secret values are then consumed into
    // our zeroizing wrappers immediately after URL creation.
    let client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_auth_uri(
            AuthUrl::new(AUTHORIZATION_ENDPOINT.to_owned())
                .map_err(|_| OAuthError::Configuration(ConfigurationError::InvalidFixedEndpoint))?,
        )
        .set_token_uri(
            TokenUrl::new(TOKEN_ENDPOINT.to_owned())
                .map_err(|_| OAuthError::Configuration(ConfigurationError::InvalidFixedEndpoint))?,
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.clone())
                .map_err(|_| OAuthError::Configuration(ConfigurationError::InvalidFixedEndpoint))?,
        );
    let state_string =
        Secret::random_string(random, STATE_RANDOM_BYTES).map_err(OAuthError::Random)?;
    let verifier_string = match Secret::random_string(random, VERIFIER_RANDOM_BYTES) {
        Ok(value) => value,
        Err(error) => {
            let mut state_string = state_string;
            state_string.zeroize();
            return Err(OAuthError::Random(error));
        }
    };
    let state = CsrfToken::new(state_string);
    let verifier = PkceCodeVerifier::new(verifier_string);
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
    let (authorization_url, state) = client
        .authorize_url(|| state)
        .add_scope(Scope::new(REQUESTED_SCOPE.to_owned()))
        .set_pkce_challenge(challenge)
        .add_extra_param("actor", REQUESTED_ACTOR)
        .url();

    Ok(AuthorizationRequest {
        authorization_url: Zeroizing::new(authorization_url.to_string()),
        redirect_uri,
        state: Secret::new(state.into_secret()),
        verifier: Secret::new(verifier.into_secret()),
    })
}

/// Secret-bearing token exchange request.
struct TokenExchangeRequest {
    client_id: String,
    redirect_uri: String,
    code: Secret,
    verifier: Secret,
}

impl TokenExchangeRequest {
    fn form_body(&self) -> Zeroizing<Vec<u8>> {
        let mut form = Serializer::new(String::new());
        form.append_pair("grant_type", "authorization_code");
        form.append_pair("client_id", &self.client_id);
        form.append_pair("redirect_uri", &self.redirect_uri);
        form.append_pair("code", self.code.as_str());
        form.append_pair("code_verifier", self.verifier.as_str());
        Zeroizing::new(form.finish().into_bytes())
    }
}

impl fmt::Debug for TokenExchangeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenExchangeRequest")
            .field("grant_type", &"authorization_code")
            .field("client_id", &"[redacted]")
            .field("redirect_uri", &self.redirect_uri)
            .field("code", &"[redacted]")
            .field("code_verifier", &"[redacted]")
            .finish()
    }
}

/// The requested app actor and validated read-only token metadata held with the
/// secrets. `requested_actor` is request metadata, not verified provider
/// identity; live identity verification belongs to P0-05.
pub struct TokenBundle {
    access_token: Secret,
    refresh_token: Secret,
    expires_in: Duration,
}

impl TokenBundle {
    /// Returns the fixed actor requested during authorization.
    ///
    /// This is request metadata, not a verified provider identity.
    #[cfg(test)]
    pub fn requested_actor(&self) -> &'static str {
        REQUESTED_ACTOR
    }

    /// Returns the validated granted scope.
    #[cfg(test)]
    pub fn scope(&self) -> &'static str {
        REQUESTED_SCOPE
    }

    /// Returns the validated positive access-token lifetime.
    #[cfg(test)]
    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }

    /// Consumes the validated bundle for the credential manager.
    ///
    /// This is intentionally crate-private: the persistence boundary may move
    /// the zeroizing buffers into its own envelope, while callers outside the
    /// crate never receive raw token strings.
    pub(crate) fn into_credential_parts(self) -> (Zeroizing<String>, Zeroizing<String>, Duration) {
        let Self {
            access_token,
            refresh_token,
            expires_in,
        } = self;
        (access_token.0, refresh_token.0, expires_in)
    }

    /// Constructs a bundle after a crate-internal response validator has
    /// checked both token values and the positive lifetime.
    pub(crate) fn from_credential_parts(
        access_token: Zeroizing<String>,
        refresh_token: Zeroizing<String>,
        expires_in: Duration,
    ) -> Self {
        Self {
            access_token: Secret::new(access_token.to_string()),
            refresh_token: Secret::new(refresh_token.to_string()),
            expires_in,
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(
        access_token: Zeroizing<String>,
        refresh_token: Zeroizing<String>,
        expires_in: Duration,
    ) -> Self {
        Self::from_credential_parts(access_token, refresh_token, expires_in)
    }
}

impl fmt::Debug for TokenBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenBundle")
            .field("access_token", &self.access_token)
            .field("refresh_token", &self.refresh_token)
            .field("expires_in", &self.expires_in)
            .field("scope", &REQUESTED_SCOPE)
            .field("requested_actor", &REQUESTED_ACTOR)
            .finish()
    }
}

/// Top-level OAuth operation failures.  Every variant is intentionally coarse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OAuthError {
    /// Local configuration was invalid.
    Configuration(ConfigurationError),
    /// Secure random generation failed.
    Random(RandomError),
    /// Listener binding failed.
    Listener(ListenerError),
    /// Browser launch failed.
    Browser(BrowserError),
    /// Callback validation failed.
    Callback(CallbackError),
    /// Token transport failed.
    TokenTransport(TokenTransportError),
    /// The token response did not satisfy the fixed contract.
    InvalidTokenResponse,
    /// The authorization deadline could not be represented.
    DeadlineOverflow,
}

impl fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => error.fmt(formatter),
            Self::Random(error) => error.fmt(formatter),
            Self::Listener(error) => error.fmt(formatter),
            Self::Browser(error) => error.fmt(formatter),
            Self::Callback(error) => error.fmt(formatter),
            Self::TokenTransport(error) => error.fmt(formatter),
            Self::InvalidTokenResponse => {
                formatter.write_str("Linear OAuth token response is invalid")
            }
            Self::DeadlineOverflow => {
                formatter.write_str("Linear OAuth authorization deadline is invalid")
            }
        }
    }
}

impl std::error::Error for OAuthError {}

/// Runs one complete authorization-code plus PKCE exchange.
///
/// The callback listener is bound before the browser launcher is called.  The
/// token transport is invoked at most once, and no token is persisted here.
fn authorize(
    config: &OAuthConfig,
    random: &mut dyn RandomSource,
    clock: &dyn Clock,
    listener_factory: &dyn CallbackListenerFactory,
    browser: &dyn BrowserLauncher,
    transport: &mut dyn TokenTransport,
) -> Result<TokenBundle, OAuthError> {
    let request = prepare_authorization(config, random)?;
    let deadline = clock
        .now()
        .checked_add(AUTHORIZATION_TIMEOUT)
        .ok_or(OAuthError::DeadlineOverflow)?;

    // Binding must happen before opening the browser so the redirect cannot race
    // a listener that has not started yet.
    let mut listener = listener_factory
        .bind(request.redirect_uri())
        .map_err(OAuthError::Listener)?;
    browser
        .launch(request.authorization_url(), deadline, clock)
        .map_err(OAuthError::Browser)?;

    let callback = listener
        .wait_for_callback(deadline, clock, request.state.as_bytes())
        .map_err(OAuthError::Callback)?;
    // The listener boundary is injectable, so enforce the callback deadline
    // again here instead of trusting an implementation to do so.
    if clock.now() >= deadline {
        return Err(OAuthError::Callback(CallbackError::Timeout));
    }
    drop(listener);
    if !constant_time_equal(callback.state.as_bytes(), request.state.as_bytes()) {
        return Err(OAuthError::Callback(CallbackError::StateMismatch));
    }

    let (code, verifier) = (callback.code, request.verifier);
    let token_request = TokenExchangeRequest {
        client_id: config.client_id.clone(),
        redirect_uri: request.redirect_uri,
        code,
        verifier,
    };

    // Keep the deadline check immediately adjacent to the one and only token
    // exchange. This closes the gap while constructing the request above.
    if clock.now() >= deadline {
        return Err(OAuthError::Callback(CallbackError::Timeout));
    }

    // This is deliberately one call.  A lost response is ambiguous and is not
    // retried by this authorization operation.
    let response = transport
        .exchange(&token_request)
        .map_err(OAuthError::TokenTransport)?;
    validate_token_response(response)
}

/// Runs the production flow with the fixed browser, listener, entropy, clock,
/// and HTTPS transport boundaries.
pub fn authorize_production(config: &OAuthConfig) -> Result<TokenBundle, OAuthError> {
    let mut random = OsRandom;
    let clock = MonotonicClock;
    let listener_factory = LoopbackCallbackListenerFactory;
    let browser = SystemBrowserLauncher;
    let mut transport = HttpsTokenTransport::new().map_err(OAuthError::TokenTransport)?;
    authorize(
        config,
        &mut random,
        &clock,
        &listener_factory,
        &browser,
        &mut transport,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTokenResponse {
    access_token: Option<Secret>,
    refresh_token: Option<Secret>,
    token_type: Option<Secret>,
    expires_in: Option<u64>,
    scope: Option<Secret>,
    /// The default marks an omitted actor as absent; custom deserialization
    /// marks an explicit JSON null as present-but-invalid.
    #[serde(default)]
    actor: ActorField,
}

#[derive(Default)]
struct ActorField {
    present: bool,
    value: Option<Secret>,
}

impl<'de> Deserialize<'de> for ActorField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self {
            present: true,
            value: Option::<Secret>::deserialize(deserializer)?,
        })
    }
}

fn validate_token_response(response: TokenTransportResponse) -> Result<TokenBundle, OAuthError> {
    if response.status != 200 {
        return Err(OAuthError::InvalidTokenResponse);
    }

    let mut raw: RawTokenResponse =
        serde_json::from_slice(&response.body).map_err(|_| OAuthError::InvalidTokenResponse)?;
    let access_token = raw
        .access_token
        .take()
        .ok_or(OAuthError::InvalidTokenResponse)?;
    let refresh_token = raw
        .refresh_token
        .take()
        .ok_or(OAuthError::InvalidTokenResponse)?;
    raw.token_type
        .as_ref()
        .map(Secret::as_str)
        .filter(|value| value.eq_ignore_ascii_case("Bearer"))
        .ok_or(OAuthError::InvalidTokenResponse)?;
    let expires_in = raw
        .expires_in
        .filter(|value| *value > 0)
        .ok_or(OAuthError::InvalidTokenResponse)?;
    raw.scope
        .as_ref()
        .map(Secret::as_str)
        .filter(|value| scope_matches(value))
        .ok_or(OAuthError::InvalidTokenResponse)?;
    if raw.actor.present
        && raw
            .actor
            .value
            .as_ref()
            .map(Secret::as_str)
            .is_none_or(|value| value != REQUESTED_ACTOR)
    {
        return Err(OAuthError::InvalidTokenResponse);
    }
    let expires_in = Duration::from_secs(expires_in);

    Ok(TokenBundle {
        access_token,
        refresh_token,
        expires_in,
    })
}

/// True when a granted scope names exactly the requested scopes, in any order,
/// separated by commas or whitespace.
pub(crate) fn scope_matches(granted: &str) -> bool {
    let mut granted: Vec<&str> = granted
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .collect();
    let mut requested: Vec<&str> = REQUESTED_SCOPE.split(',').collect();
    granted.sort_unstable();
    granted.dedup();
    requested.sort_unstable();
    granted == requested
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let Ok(left) = std::str::from_utf8(left) else {
        return false;
    };
    let Ok(right) = std::str::from_utf8(right) else {
        return false;
    };
    TimingResistantCsrfToken::equal(left, right)
}

fn callback_uri(port: u16) -> String {
    format!("http://{CALLBACK_HOST}:{port}{CALLBACK_PATH}")
}

fn parse_callback_uri(uri: &str) -> Option<u16> {
    let prefix = format!("http://{CALLBACK_HOST}:");
    let port = uri.strip_prefix(&prefix)?.strip_suffix(CALLBACK_PATH)?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port = port.parse::<u16>().ok()?;
    (port != 0).then_some(port)
}

fn accept_until(
    listener: &TcpListener,
    deadline: Instant,
    clock: &dyn Clock,
) -> Result<(TcpStream, std::net::SocketAddr), CallbackError> {
    loop {
        let now = clock.now();
        if now >= deadline {
            return Err(CallbackError::Timeout);
        }
        match listener.accept() {
            Ok(connection) => return Ok(connection),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(now);
                std::thread::sleep(remaining.min(CALLBACK_POLL_INTERVAL));
            }
            Err(_) => return Err(CallbackError::IoFailure),
        }
    }
}

fn read_callback(
    stream: &mut TcpStream,
    deadline: Instant,
    clock: &dyn Clock,
) -> Result<Callback, CallbackError> {
    let mut request = read_bounded_request(stream, deadline, clock)?;
    let result = parse_callback_request(&request);
    request.zeroize();
    result
}

fn read_bounded_request(
    stream: &mut TcpStream,
    deadline: Instant,
    clock: &dyn Clock,
) -> Result<Zeroizing<Vec<u8>>, CallbackError> {
    let mut request = Zeroizing::new(Vec::with_capacity(MAX_CALLBACK_REQUEST_BYTES.min(1024)));
    let mut buffer = Zeroizing::new([0_u8; 1024]);
    loop {
        if request.len() >= MAX_CALLBACK_REQUEST_BYTES {
            return Err(CallbackError::OversizedRequest);
        }
        let now = clock.now();
        if now >= deadline {
            return Err(CallbackError::Timeout);
        }
        let remaining = deadline.saturating_duration_since(now);
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| CallbackError::IoFailure)?;
        let read = match stream.read(&mut buffer[..]) {
            Ok(read) => read,
            Err(error)
                if error.kind() == io::ErrorKind::TimedOut
                    || error.kind() == io::ErrorKind::WouldBlock =>
            {
                if clock.now() >= deadline {
                    return Err(CallbackError::Timeout);
                }
                continue;
            }
            Err(_) => return Err(CallbackError::IoFailure),
        };
        if read == 0 {
            return Err(CallbackError::MalformedRequest);
        }
        if request.len().saturating_add(read) > MAX_CALLBACK_REQUEST_BYTES {
            return Err(CallbackError::OversizedRequest);
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
    }
}

fn parse_callback_request(request: &[u8]) -> Result<Callback, CallbackError> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_CALLBACK_HEADERS];
    let mut parsed = httparse::Request::new(&mut headers);
    let header_length = match parsed.parse(request) {
        Ok(Status::Complete(length)) => length,
        Ok(Status::Partial) => return Err(CallbackError::MalformedRequest),
        Err(httparse::Error::TooManyHeaders) => {
            return Err(CallbackError::OversizedRequest);
        }
        Err(_) => return Err(CallbackError::MalformedRequest),
    };
    if header_length > MAX_CALLBACK_REQUEST_BYTES {
        return Err(CallbackError::OversizedRequest);
    }
    if header_length != request.len() {
        return Err(CallbackError::MalformedRequest);
    }
    let mut content_length_seen = false;
    for header in parsed.headers.iter() {
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(CallbackError::MalformedRequest);
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            if content_length_seen {
                return Err(CallbackError::MalformedRequest);
            }
            content_length_seen = true;
            let length = std::str::from_utf8(header.value)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .ok_or(CallbackError::MalformedRequest)?;
            if length != 0 {
                return Err(CallbackError::MalformedRequest);
            }
        }
    }
    if parsed.method != Some("GET") {
        return Err(CallbackError::WrongMethod);
    }
    if parsed.version != Some(1) {
        return Err(CallbackError::MalformedRequest);
    }
    let target = parsed.path.ok_or(CallbackError::MalformedRequest)?;
    if target.contains('#') {
        return Err(CallbackError::WrongPath);
    }
    let query = target
        .strip_prefix(CALLBACK_PATH)
        .and_then(|value| value.strip_prefix('?'))
        .ok_or(CallbackError::WrongPath)?;
    if query.is_empty() {
        return Err(CallbackError::MissingParameter);
    }
    parse_callback_query(query)
}

fn parse_callback_query(query: &str) -> Result<Callback, CallbackError> {
    let mut code: Option<Secret> = None;
    let mut state: Option<Secret> = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            return Err(CallbackError::InvalidParameter);
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or(CallbackError::InvalidParameter)?;
        let key = decode_query_component(key)?;
        let mut value = decode_query_component(value)?;
        if key == "error" || key == "error_description" {
            value.zeroize();
            return Err(CallbackError::ErrorCallback);
        }
        match key.as_str() {
            "code" => {
                if code.is_some() {
                    value.zeroize();
                    return Err(CallbackError::DuplicateParameter);
                }
                code = Some(Secret::try_new(value).ok_or(CallbackError::InvalidParameter)?);
            }
            "state" => {
                if state.is_some() {
                    value.zeroize();
                    return Err(CallbackError::DuplicateParameter);
                }
                state = Some(Secret::try_new(value).ok_or(CallbackError::InvalidParameter)?);
            }
            _ => {
                value.zeroize();
                return Err(CallbackError::InvalidParameter);
            }
        }
    }
    let code = code.ok_or(CallbackError::MissingParameter)?;
    let state = state.ok_or(CallbackError::MissingParameter)?;
    Ok(Callback { code, state })
}

fn decode_query_component(component: &str) -> Result<String, CallbackError> {
    let bytes = component.as_bytes();
    let mut decoded = Zeroizing::new(Vec::with_capacity(bytes.len()));
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(CallbackError::InvalidParameter);
                }
                let high = hex_value(bytes[index + 1]).ok_or(CallbackError::InvalidParameter)?;
                let low = hex_value(bytes[index + 2]).ok_or(CallbackError::InvalidParameter)?;
                decoded.push((high << 4) | low);
                index += 2;
            }
            byte if byte.is_ascii() => decoded.push(byte),
            _ => return Err(CallbackError::InvalidParameter),
        }
        index += 1;
    }
    let mut decoded = decoded;
    let decoded = std::mem::take(&mut *decoded);
    match String::from_utf8(decoded) {
        Ok(value) => Ok(value),
        Err(error) => {
            let mut bytes = error.into_bytes();
            bytes.zeroize();
            Err(CallbackError::InvalidParameter)
        }
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_valid_secret_value(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn write_callback_response(
    stream: &mut TcpStream,
    success: bool,
    deadline: Instant,
    clock: &dyn Clock,
) {
    let body = if success {
        CALLBACK_SUCCESS_BODY
    } else {
        CALLBACK_FAILURE_BODY
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let now = clock.now();
    let timeout = if now < deadline {
        deadline.saturating_duration_since(now)
    } else {
        // A callback parsed at the deadline must still receive the fixed
        // failure response. Keep this best-effort write bounded independently
        // of the expired authorization deadline.
        CALLBACK_RESPONSE_TIMEOUT
    };
    let _ = stream.set_write_timeout(Some(timeout));
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundedReadError {
    Io,
    TooLarge,
}

fn read_bounded<R: Read>(
    reader: &mut R,
    limit: usize,
) -> Result<Zeroizing<Vec<u8>>, BoundedReadError> {
    let mut body = Zeroizing::new(Vec::with_capacity(limit.min(4096)));
    let mut buffer = Zeroizing::new([0_u8; 4096]);
    loop {
        let read = reader
            .read(&mut buffer[..])
            .map_err(|_| BoundedReadError::Io)?;
        if read == 0 {
            return Ok(body);
        }
        if body.len().saturating_add(read) > limit {
            return Err(BoundedReadError::TooLarge);
        }
        body.extend_from_slice(&buffer[..read]);
    }
}

fn read_bounded_response(
    mut response: oauth2::reqwest::blocking::Response,
) -> Result<Zeroizing<Vec<u8>>, TokenTransportError> {
    read_bounded(&mut response, MAX_TOKEN_RESPONSE_BYTES).map_err(|error| match error {
        BoundedReadError::Io => TokenTransportError::RequestFailed,
        BoundedReadError::TooLarge => TokenTransportError::ResponseTooLarge,
    })
}

struct SecretBody {
    bytes: Zeroizing<Vec<u8>>,
    offset: usize,
}

/// Uses oauth2's timing-resistant secret comparison while zeroizing the
/// temporary token values after the comparison.
struct TimingResistantCsrfToken(Option<CsrfToken>);

impl TimingResistantCsrfToken {
    fn equal(left: &str, right: &str) -> bool {
        let left = Self(Some(CsrfToken::new(left.to_owned())));
        let right = Self(Some(CsrfToken::new(right.to_owned())));
        left.0.as_ref().expect("CSRF token present")
            == right.0.as_ref().expect("CSRF token present")
    }
}

impl Drop for TimingResistantCsrfToken {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            let mut value = token.into_secret();
            value.zeroize();
        }
    }
}

impl SecretBody {
    fn new(bytes: Zeroizing<Vec<u8>>) -> Self {
        Self { bytes, offset: 0 }
    }
}

impl Read for SecretBody {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        let remaining = self.bytes.len().saturating_sub(self.offset);
        let length = remaining.min(destination.len());
        if length == 0 {
            return Ok(0);
        }
        destination[..length].copy_from_slice(&self.bytes[self.offset..self.offset + length]);
        self.offset += length;
        Ok(length)
    }
}

struct Secret(Zeroizing<String>);

impl Secret {
    fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    fn try_new(value: String) -> Option<Self> {
        if is_valid_secret_value(&value) {
            Some(Self::new(value))
        } else {
            let mut value = value;
            value.zeroize();
            None
        }
    }

    fn random_string(random: &mut dyn RandomSource, bytes: usize) -> Result<String, RandomError> {
        let mut random_bytes = Zeroizing::new(vec![0_u8; bytes]);
        random.fill_bytes(&mut random_bytes)?;
        let encoded = URL_SAFE_NO_PAD.encode(&*random_bytes);
        if is_valid_secret_value(&encoded) {
            Ok(encoded)
        } else {
            let mut encoded = encoded;
            encoded.zeroize();
            Err(RandomError::Unavailable)
        }
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Secret::try_new(value).ok_or_else(|| serde::de::Error::custom("invalid secret value"))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::net::TcpStream;
    use std::rc::Rc;

    const CLIENT_ID: &str = "synthetic-client";
    const ACCESS_TOKEN: &str = "synthetic-access-token";
    const REFRESH_TOKEN: &str = "synthetic-refresh-token";

    struct FixedRandom {
        next: u8,
    }

    impl FixedRandom {
        fn new() -> Self {
            Self { next: 1 }
        }
    }

    impl RandomSource for FixedRandom {
        fn fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), RandomError> {
            destination.fill(self.next);
            self.next = self.next.wrapping_add(1);
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    struct FixedClock(Instant);

    impl Clock for FixedClock {
        fn now(&self) -> Instant {
            self.0
        }
    }

    struct SequenceClock {
        values: RefCell<VecDeque<Instant>>,
        fallback: Instant,
    }

    impl SequenceClock {
        fn new(values: impl IntoIterator<Item = Instant>, fallback: Instant) -> Self {
            Self {
                values: RefCell::new(values.into_iter().collect()),
                fallback,
            }
        }
    }

    impl Clock for SequenceClock {
        fn now(&self) -> Instant {
            self.values
                .borrow_mut()
                .pop_front()
                .unwrap_or(self.fallback)
        }
    }

    struct RecordingBrowser {
        events: Rc<RefCell<Vec<&'static str>>>,
        result: Result<(), BrowserError>,
    }

    impl BrowserLauncher for RecordingBrowser {
        fn launch(
            &self,
            authorization_url: &str,
            _deadline: Instant,
            _clock: &dyn Clock,
        ) -> Result<(), BrowserError> {
            self.events.borrow_mut().push("browser");
            assert!(authorization_url.starts_with(AUTHORIZATION_ENDPOINT));
            self.result
        }
    }

    struct RecordingFactory {
        events: Rc<RefCell<Vec<&'static str>>>,
        callback: RefCell<Option<Result<Callback, CallbackError>>>,
    }

    struct RecordingListener {
        events: Rc<RefCell<Vec<&'static str>>>,
        callback: RefCell<Option<Result<Callback, CallbackError>>>,
    }

    impl CallbackListener for RecordingListener {
        fn wait_for_callback(
            &mut self,
            _deadline: Instant,
            _clock: &dyn Clock,
            expected_state: &[u8],
        ) -> Result<Callback, CallbackError> {
            self.events.borrow_mut().push("listener");
            let callback = self
                .callback
                .borrow_mut()
                .take()
                .expect("test callback result");
            if let Ok(callback) = &callback {
                assert!(!expected_state.is_empty());
                assert!(!callback.state.as_bytes().is_empty());
            }
            callback
        }
    }

    impl CallbackListenerFactory for RecordingFactory {
        fn bind(&self, redirect_uri: &str) -> Result<Box<dyn CallbackListener>, ListenerError> {
            assert!(redirect_uri.starts_with("http://127.0.0.1:"));
            self.events.borrow_mut().push("bind");
            Ok(Box::new(RecordingListener {
                events: Rc::clone(&self.events),
                callback: RefCell::new(self.callback.borrow_mut().take()),
            }))
        }
    }

    struct RecordingTransport {
        calls: Cell<usize>,
        response: Option<TokenTransportResponse>,
    }

    struct FakeChild {
        polls: VecDeque<io::Result<Option<bool>>>,
        kill_calls: usize,
        wait_calls: usize,
        running: bool,
    }

    impl FakeChild {
        fn new(polls: impl IntoIterator<Item = io::Result<Option<bool>>>) -> Self {
            Self {
                polls: polls.into_iter().collect(),
                kill_calls: 0,
                wait_calls: 0,
                running: true,
            }
        }
    }

    impl ChildProcess for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<bool>> {
            self.polls
                .pop_front()
                .unwrap_or(Ok(None))
                .inspect(|status| {
                    if status.is_some() {
                        self.running = false;
                    }
                })
        }

        fn kill(&mut self) -> io::Result<()> {
            self.kill_calls += 1;
            Ok(())
        }

        fn wait(&mut self) -> io::Result<bool> {
            self.wait_calls += 1;
            self.running = false;
            Ok(false)
        }
    }

    impl TokenTransport for RecordingTransport {
        fn exchange(
            &mut self,
            request: &TokenExchangeRequest,
        ) -> Result<TokenTransportResponse, TokenTransportError> {
            self.calls.set(self.calls.get() + 1);
            assert_eq!(request.client_id, CLIENT_ID);
            assert_eq!(
                request.redirect_uri,
                "http://127.0.0.1:43871/oauth/callback"
            );
            assert_eq!(request.code.as_str(), "synthetic-code");
            assert!(!request.verifier.as_str().is_empty());
            self.response
                .take()
                .ok_or(TokenTransportError::RequestFailed)
        }
    }

    struct Harness<C> {
        config: OAuthConfig,
        random: FixedRandom,
        clock: C,
        events: Rc<RefCell<Vec<&'static str>>>,
        factory: RecordingFactory,
        browser: RecordingBrowser,
        transport: RecordingTransport,
    }

    impl<C: Clock> Harness<C> {
        fn new(clock: C) -> Self {
            let events = Rc::new(RefCell::new(Vec::new()));
            Self {
                config: OAuthConfig::new(CLIENT_ID).expect("config"),
                random: FixedRandom::new(),
                clock,
                factory: RecordingFactory {
                    events: Rc::clone(&events),
                    callback: RefCell::new(None),
                },
                browser: RecordingBrowser {
                    events: Rc::clone(&events),
                    result: Ok(()),
                },
                transport: RecordingTransport {
                    calls: Cell::new(0),
                    response: Some(valid_token_response()),
                },
                events,
            }
        }

        fn matching(clock: C) -> Self {
            let harness = Self::new(clock);
            let mut random = FixedRandom::new();
            let prepared = prepare_authorization(&harness.config, &mut random).expect("request");
            let callback = Callback::new("synthetic-code", prepared.state.as_str())
                .expect("matching callback");
            harness.factory.callback.replace(Some(Ok(callback)));
            harness
        }

        fn set_callback(&mut self, callback: Result<Callback, CallbackError>) {
            self.factory.callback.replace(Some(callback));
        }

        fn set_browser_result(&mut self, result: Result<(), BrowserError>) {
            self.browser.result = result;
        }

        fn remove_response(&mut self) {
            self.transport.response = None;
        }

        fn authorize(&mut self) -> Result<TokenBundle, OAuthError> {
            authorize(
                &self.config,
                &mut self.random,
                &self.clock,
                &self.factory,
                &self.browser,
                &mut self.transport,
            )
        }
    }

    fn token_response(body: &str) -> TokenTransportResponse {
        TokenTransportResponse::new(200, body.as_bytes().to_vec()).expect("test response")
    }

    fn token_response_with_status(status: u16, body: &str) -> TokenTransportResponse {
        TokenTransportResponse::new(status, body.as_bytes().to_vec()).expect("test response")
    }

    fn valid_token_response() -> TokenTransportResponse {
        token_response(&format!(
            "{{\"access_token\":\"{ACCESS_TOKEN}\",\"refresh_token\":\"{REFRESH_TOKEN}\",\"token_type\":\"Bearer\",\"expires_in\":60,\"scope\":\"read,write,app:assignable\"}}"
        ))
    }

    fn request_bytes(target: &str) -> Vec<u8> {
        format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").into_bytes()
    }

    fn callback_request(query: &str) -> Vec<u8> {
        request_bytes(&format!("{CALLBACK_PATH}?{query}"))
    }

    fn ephemeral_listener() -> LoopbackCallbackListener {
        let listener = TcpListener::bind((CALLBACK_HOST, 0)).expect("ephemeral listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        LoopbackCallbackListener {
            listener,
            consumed: false,
        }
    }

    fn run_callback(
        listener: &mut LoopbackCallbackListener,
        query: &str,
        deadline: Instant,
        clock: &dyn Clock,
        expected_state: &[u8],
    ) -> (Result<Callback, CallbackError>, Vec<u8>) {
        let mut stream = TcpStream::connect(listener.local_addr().expect("address"))
            .expect("callback connection");
        stream
            .write_all(&callback_request(query))
            .expect("callback write");
        let result = listener.wait_for_callback(deadline, clock, expected_state);
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .expect("callback response");
        (result, response)
    }

    #[test]
    fn browser_child_success_and_failure_are_reaped_by_poll() {
        let start = Instant::now();
        let deadline = start + AUTHORIZATION_TIMEOUT;
        let clock = FixedClock(start);

        let mut successful_child = FakeChild::new([Ok(Some(true))]);
        assert_eq!(
            wait_for_browser_child(&mut successful_child, deadline, &clock, |_| {}),
            Ok(())
        );
        assert_eq!(successful_child.kill_calls, 0);
        assert_eq!(successful_child.wait_calls, 0);
        assert!(!successful_child.running);

        let mut failed_child = FakeChild::new([Ok(Some(false))]);
        assert_eq!(
            wait_for_browser_child(&mut failed_child, deadline, &clock, |_| {}),
            Err(BrowserError::LaunchFailed)
        );
        assert_eq!(failed_child.kill_calls, 0);
        assert_eq!(failed_child.wait_calls, 0);
        assert!(!failed_child.running);
    }

    #[test]
    fn browser_child_timeout_kills_and_waits_for_no_remaining_child() {
        let start = Instant::now();
        let deadline = start + AUTHORIZATION_TIMEOUT;
        let clock = SequenceClock::new([start, deadline], deadline);
        let mut child = FakeChild::new([Ok(None)]);

        assert_eq!(
            wait_for_browser_child(&mut child, deadline, &clock, |_| {}),
            Err(BrowserError::LaunchFailed)
        );
        assert_eq!(child.kill_calls, 1);
        assert_eq!(child.wait_calls, 1);
        assert!(!child.running, "timed out child must be reaped");
    }

    #[test]
    fn authorization_url_is_fixed_scope_actor_pkce_and_redirect() {
        let config = OAuthConfig::new(CLIENT_ID).expect("config");
        let mut random = FixedRandom::new();
        let request = prepare_authorization(&config, &mut random).expect("authorization request");
        let url = oauth2::url::Url::parse(request.authorization_url()).expect("authorization URL");
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("linear.app"));
        assert_eq!(url.path(), "/oauth/authorize");
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(query.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:43871/oauth/callback")
        );
        assert_eq!(
            query.get("scope").map(String::as_str),
            Some(REQUESTED_SCOPE)
        );
        assert_eq!(query.get("actor").map(String::as_str), Some("app"));
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(query.contains_key("state"));
        assert!(query.contains_key("code_challenge"));
        assert!(!request.authorization_url().contains("client_secret"));
        assert!(!request.authorization_url().contains("scope=read%20write"));
    }

    #[test]
    fn state_and_verifier_have_high_entropy_pkce_shape() {
        let config = OAuthConfig::new(CLIENT_ID).expect("config");
        let mut random = FixedRandom::new();
        let request = prepare_authorization(&config, &mut random).expect("authorization request");
        let url = oauth2::url::Url::parse(request.authorization_url()).expect("authorization URL");
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        let state = query.get("state").expect("state");
        let challenge = query.get("code_challenge").expect("challenge");
        assert_eq!(state.len(), 43);
        assert_eq!(challenge.len(), 43);
        assert!(
            state
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        );
        assert!(
            challenge
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        );
        assert_eq!(
            request.redirect_uri(),
            "http://127.0.0.1:43871/oauth/callback"
        );
    }

    #[test]
    fn authorization_binds_before_browser_and_exchanges_once() {
        let mut harness = Harness::new(FixedClock(Instant::now()));
        harness.set_callback(Ok(
            Callback::new("synthetic-code", "synthetic-state").expect("callback")
        ));
        // The fake callback state deliberately mismatches. This verifies that
        // an injected listener cannot bypass state validation or reach transport.
        let result = harness.authorize();
        assert!(matches!(
            result,
            Err(OAuthError::Callback(CallbackError::StateMismatch))
        ));
        assert_eq!(harness.transport.calls.get(), 0);
        assert_eq!(
            harness.events.borrow().as_slice(),
            ["bind", "browser", "listener"]
        );
    }

    #[test]
    fn browser_failure_prevents_callback_and_token_exchange() {
        let mut harness = Harness::matching(FixedClock(Instant::now()));
        harness.set_browser_result(Err(BrowserError::LaunchFailed));
        assert!(matches!(
            harness.authorize(),
            Err(OAuthError::Browser(BrowserError::LaunchFailed))
        ));
        assert_eq!(harness.events.borrow().as_slice(), ["bind", "browser"]);
        assert_eq!(harness.transport.calls.get(), 0);
    }

    #[test]
    fn injected_callback_at_deadline_prevents_token_exchange() {
        let start = Instant::now();
        let deadline = start + AUTHORIZATION_TIMEOUT;
        let mut harness = Harness::matching(SequenceClock::new([start, deadline], deadline));
        assert!(matches!(
            harness.authorize(),
            Err(OAuthError::Callback(CallbackError::Timeout))
        ));
        assert_eq!(harness.transport.calls.get(), 0);
    }

    #[test]
    fn deadline_is_checked_immediately_before_token_exchange() {
        let start = Instant::now();
        let deadline = start + AUTHORIZATION_TIMEOUT;
        // The callback returns before the boundary; the final clock reading
        // reaches it exactly while the token request is being prepared.
        let mut harness = Harness::matching(SequenceClock::new([start, start, deadline], deadline));
        assert!(matches!(
            harness.authorize(),
            Err(OAuthError::Callback(CallbackError::Timeout))
        ));
        assert_eq!(harness.transport.calls.get(), 0);
    }

    #[test]
    fn valid_injected_callback_makes_one_token_request() {
        let mut harness = Harness::matching(FixedClock(Instant::now()));
        let result = harness.authorize().expect("authorization");
        assert_eq!(result.requested_actor(), "app");
        assert_eq!(result.scope(), REQUESTED_SCOPE);
        assert_eq!(result.expires_in(), Duration::from_secs(60));
        assert_eq!(harness.transport.calls.get(), 1);
    }

    #[test]
    fn token_form_has_exact_authorization_code_fields() {
        let request = TokenExchangeRequest {
            client_id: CLIENT_ID.to_owned(),
            redirect_uri: "http://127.0.0.1:43871/oauth/callback".to_owned(),
            code: Secret::new("synthetic-code".to_owned()),
            verifier: Secret::new("synthetic-verifier".to_owned()),
        };
        let body = request.form_body();
        let fields = oauth2::url::form_urlencoded::parse(body.as_slice())
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            fields.keys().cloned().collect::<Vec<_>>(),
            vec![
                "client_id".to_owned(),
                "code".to_owned(),
                "code_verifier".to_owned(),
                "grant_type".to_owned(),
                "redirect_uri".to_owned(),
            ]
        );
        assert_eq!(
            fields.get("grant_type").map(String::as_str),
            Some("authorization_code")
        );
        assert_eq!(fields.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            fields.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:43871/oauth/callback")
        );
        assert_eq!(
            fields.get("code").map(String::as_str),
            Some("synthetic-code")
        );
        assert_eq!(
            fields.get("code_verifier").map(String::as_str),
            Some("synthetic-verifier")
        );
        assert!(!fields.contains_key("client_secret"));
        assert!(!fields.contains_key("scope"));
        assert!(!fields.contains_key("actor"));
    }

    #[test]
    fn ambiguous_transport_failure_is_not_retried() {
        let mut harness = Harness::matching(FixedClock(Instant::now()));
        harness.remove_response();
        let error = harness.authorize().expect_err("transport must fail");
        assert_eq!(
            error,
            OAuthError::TokenTransport(TokenTransportError::RequestFailed)
        );
        assert_eq!(harness.transport.calls.get(), 1);
    }

    #[test]
    fn bounded_reader_accepts_exact_limit_and_rejects_over_limit() {
        let mut exact = Cursor::new(b"1234".to_vec());
        let body = read_bounded(&mut exact, 4).expect("exact limit is accepted");
        assert_eq!(&*body, b"1234");

        let mut over = Cursor::new(b"12345".to_vec());
        assert_eq!(read_bounded(&mut over, 4), Err(BoundedReadError::TooLarge));
    }

    #[test]
    fn invalid_utf8_percent_decoding_is_rejected() {
        assert!(matches!(
            decode_query_component("%ff"),
            Err(CallbackError::InvalidParameter)
        ));
    }

    #[test]
    fn callback_parser_accepts_valid_origin_form() {
        let callback = parse_callback_request(&callback_request(
            "code=synthetic-code&state=synthetic-state",
        ))
        .expect("callback");
        assert_eq!(callback.code.as_str(), "synthetic-code");
        assert_eq!(callback.state.as_str(), "synthetic-state");
    }

    #[test]
    fn callback_parser_rejects_security_boundary_cases() {
        let cases = [
            (
                request_bytes("/oauth/callback?code=one&code=two&state=state"),
                Some(CallbackError::DuplicateParameter),
            ),
            (
                request_bytes("/oauth/callback?code=one"),
                Some(CallbackError::MissingParameter),
            ),
            (
                request_bytes("/oauth/callback?code=one&state=bad%2"),
                Some(CallbackError::InvalidParameter),
            ),
            (
                request_bytes("/oauth/callback?error=denied&state=state"),
                Some(CallbackError::ErrorCallback),
            ),
            (
                request_bytes("/oauth/callback?code=one&state=state&extra=value"),
                Some(CallbackError::InvalidParameter),
            ),
            (
                b"POST /oauth/callback?code=one&state=state HTTP/1.1\r\n\r\n".to_vec(),
                Some(CallbackError::WrongMethod),
            ),
            (
                request_bytes("/not-callback?code=one&state=state"),
                Some(CallbackError::WrongPath),
            ),
            (
                request_bytes("http://127.0.0.1/oauth/callback?code=one&state=state"),
                Some(CallbackError::WrongPath),
            ),
            (
                b"GET /oauth/callback?code=one&state=state HTTP/1.1\r\nContent-Length: 1\r\n\r\n"
                    .to_vec(),
                Some(CallbackError::MalformedRequest),
            ),
            (
                b"GET /oauth/callback?code=one&state=state HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"
                    .to_vec(),
                Some(CallbackError::MalformedRequest),
            ),
        ];
        for (request, expected) in cases {
            assert!(matches!(
                parse_callback_request(&request),
                Err(error) if error == expected.expect("error")
            ));
        }
        let oversized = format!(
            "GET /oauth/callback?{} HTTP/1.1\r\n\r\n",
            "a".repeat(MAX_CALLBACK_REQUEST_BYTES)
        );
        assert!(matches!(
            parse_callback_request(oversized.as_bytes()),
            Err(CallbackError::OversizedRequest)
        ));
        let malformed = b"GET /oauth/callback?code=one&state=state HTTP/1.1\r\nBroken\r\n\r\n";
        assert!(matches!(
            parse_callback_request(malformed),
            Err(CallbackError::MalformedRequest)
        ));
        let mut body = request_bytes("/oauth/callback?code=one&state=state");
        body.extend_from_slice(b"unexpected-body");
        assert!(matches!(
            parse_callback_request(&body),
            Err(CallbackError::MalformedRequest)
        ));
    }

    #[test]
    fn loopback_listener_is_single_use_and_writes_fixed_response() {
        let mut listener = ephemeral_listener();
        let expected_state = b"synthetic-state";
        let clock = MonotonicClock;
        let (result, response) = run_callback(
            &mut listener,
            "code=synthetic-code&state=synthetic-state",
            Instant::now() + Duration::from_secs(2),
            &clock,
            expected_state,
        );
        let callback = result.expect("valid callback");
        assert_eq!(callback.code.as_str(), "synthetic-code");
        assert_eq!(response, format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\nAuthorization complete. You may close this window.",
            CALLBACK_SUCCESS_BODY.len()
        ).as_bytes());
        assert!(matches!(
            listener.wait_for_callback(
                Instant::now() + Duration::from_secs(2),
                &clock,
                expected_state
            ),
            Err(CallbackError::AlreadyConsumed)
        ));
    }

    #[test]
    fn loopback_listener_rejects_mismatch_with_fixed_failure_response() {
        let mut listener = ephemeral_listener();
        let clock = MonotonicClock;
        let (result, response) = run_callback(
            &mut listener,
            "code=synthetic-code&state=unexpected-state",
            Instant::now() + Duration::from_secs(2),
            &clock,
            b"synthetic-state",
        );
        assert!(matches!(result, Err(CallbackError::StateMismatch)));
        assert_eq!(
            response,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\nAuthorization failed. You may close this window.",
                CALLBACK_FAILURE_BODY.len()
            )
            .as_bytes()
        );
        assert!(
            !response
                .windows(b"unexpected-state".len())
                .any(|window| { window == b"unexpected-state" })
        );
    }

    #[test]
    fn loopback_listener_rejects_callback_completed_at_deadline() {
        let mut listener = ephemeral_listener();
        let start = Instant::now();
        let deadline = start + AUTHORIZATION_TIMEOUT;
        let clock = SequenceClock::new([start, start, deadline], deadline);

        let (result, response) = run_callback(
            &mut listener,
            "code=synthetic-code&state=synthetic-state",
            deadline,
            &clock,
            b"synthetic-state",
        );
        assert!(matches!(result, Err(CallbackError::Timeout)));
        assert!(response.ends_with(CALLBACK_FAILURE_BODY));
        assert!(!response.ends_with(CALLBACK_SUCCESS_BODY));
    }

    #[test]
    fn loopback_listener_timeout_consumes_callback_without_waiting() {
        let mut listener = ephemeral_listener();
        let now = Instant::now();
        let clock = FixedClock(now);
        assert!(matches!(
            listener.wait_for_callback(now, &clock, b"synthetic-state"),
            Err(CallbackError::Timeout)
        ));
        assert!(matches!(
            listener.wait_for_callback(now, &clock, b"synthetic-state"),
            Err(CallbackError::AlreadyConsumed)
        ));
    }

    #[test]
    fn token_response_validation_is_strict_and_redacted() {
        let valid = validate_token_response(valid_token_response()).expect("valid response");
        assert_eq!(valid.requested_actor(), REQUESTED_ACTOR);
        let actor_app = token_response(
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":1,"scope":"read,write,app:assignable","actor":"app"}"#,
        );
        assert_eq!(
            validate_token_response(actor_app)
                .expect("explicit app actor")
                .requested_actor(),
            REQUESTED_ACTOR
        );
        let invalid = [
            r#"{"access_token":"access","token_type":"Bearer","expires_in":1,"scope":"read,write,app:assignable"}"#,
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Basic","expires_in":1,"scope":"read,write,app:assignable"}"#,
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":0,"scope":"read,write,app:assignable"}"#,
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":1,"scope":"read write"}"#,
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":1}"#,
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":1,"scope":"read,write,app:assignable","actor":null}"#,
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":1,"scope":"read,write,app:assignable","actor":"user"}"#,
            r#"not-json"#,
            r#"{"access_token":"first","access_token":"second","refresh_token":"refresh","token_type":"Bearer","expires_in":1,"scope":"read,write,app:assignable"}"#,
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":1,"scope":"read,write,app:assignable","actor":"app","actor":"app"}"#,
            r#"{"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":1,"scope":"read,write,app:assignable","unexpected":"value"}"#,
        ];
        for body in invalid {
            assert!(matches!(
                validate_token_response(token_response(body)),
                Err(OAuthError::InvalidTokenResponse)
            ));
        }
        let created = token_response_with_status(
            201,
            &format!(
                "{{\"access_token\":\"{ACCESS_TOKEN}\",\"refresh_token\":\"{REFRESH_TOKEN}\",\"token_type\":\"Bearer\",\"expires_in\":60,\"scope\":\"read,write,app:assignable\"}}"
            ),
        );
        assert!(matches!(
            validate_token_response(created),
            Err(OAuthError::InvalidTokenResponse)
        ));
        let oversized = TokenTransportResponse::new(200, vec![b'x'; MAX_TOKEN_RESPONSE_BYTES + 1]);
        assert!(matches!(
            oversized,
            Err(TokenTransportError::ResponseTooLarge)
        ));
        let debug = format!("{:?}", valid);
        assert!(!debug.contains(ACCESS_TOKEN));
        assert!(!debug.contains(REFRESH_TOKEN));
        assert!(!format!("{}", OAuthError::InvalidTokenResponse).contains("secret-provider-error"));
    }

    #[test]
    fn secret_bearing_diagnostics_are_redacted() {
        let request = TokenExchangeRequest {
            client_id: CLIENT_ID.to_owned(),
            redirect_uri: "http://127.0.0.1:43871/oauth/callback".to_owned(),
            code: Secret::new("synthetic-code".to_owned()),
            verifier: Secret::new("synthetic-verifier".to_owned()),
        };
        let request_debug = format!("{:?}", request);
        assert!(!request_debug.contains("synthetic-code"));
        assert!(!request_debug.contains("synthetic-verifier"));

        let response = token_response("secret-provider-body");
        assert!(!format!("{:?}", response).contains("secret-provider-body"));

        let config = OAuthConfig::new(CLIENT_ID).expect("config");
        let mut random = FixedRandom::new();
        let authorization = prepare_authorization(&config, &mut random).expect("request");
        assert!(!format!("{:?}", authorization).contains(authorization.authorization_url()));
        assert!(!format!("{}", OAuthError::InvalidTokenResponse).contains("secret-provider-body"));
    }

    #[test]
    fn invalid_production_port_is_rejected() {
        let configured = OAuthConfig::with_callback_port(CLIENT_ID, 43123).expect("config");
        assert_eq!(configured.callback_port(), 43123);
        assert_eq!(
            configured.redirect_uri(),
            "http://127.0.0.1:43123/oauth/callback"
        );
        assert_eq!(
            OAuthConfig::with_callback_port(CLIENT_ID, 0),
            Err(ConfigurationError::InvalidCallbackPort)
        );
        assert_eq!(OAuthConfig::new(""), Err(ConfigurationError::EmptyClientId));
        assert_eq!(
            OAuthConfig::new("bad\nclient"),
            Err(ConfigurationError::InvalidClientId)
        );
        assert_eq!(
            parse_callback_uri("http://localhost:43871/oauth/callback"),
            None
        );
        assert_eq!(
            parse_callback_uri("https://127.0.0.1:43871/oauth/callback"),
            None
        );
        assert_eq!(
            parse_callback_uri("http://127.0.0.1:0/oauth/callback"),
            None
        );
        assert_eq!(parse_callback_uri("http://127.0.0.1:43871/other"), None);
    }
}
