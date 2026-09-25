//! One GraphQL request to Linear, bounded and without redirects or retries.
//!
//! Reads go through the credential manager's verified-read lease: every read
//! selects `viewer { id app isMe }`, and the response is accepted only when
//! the viewer is an app actor, whose ID the manager binds to the credential.
//! Writes take a lease only after that binding exists.

use std::time::Duration;

use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::credentials::CredentialManager;
use super::{ApiError, VerifiedReadOutcome};

pub const GRAPHQL_ENDPOINT: &str = "https://api.linear.app/graphql";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_MESSAGE_CHARS: usize = 200;

/// Sends one GraphQL operation and returns its `data` object.
pub trait Transport {
    fn execute(
        &mut self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> Result<Value, ApiError>;
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn execute(
        &mut self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> Result<Value, ApiError> {
        (**self).execute(operation, query, variables, write)
    }
}

/// The production transport: HTTPS to Linear with the Keychain-held token.
pub struct HttpsTransport {
    manager: CredentialManager,
    client: oauth2::reqwest::blocking::Client,
}

impl HttpsTransport {
    pub fn new(manager: CredentialManager) -> Result<Self, ApiError> {
        let client = oauth2::reqwest::blocking::ClientBuilder::new()
            .https_only(true)
            .redirect(oauth2::reqwest::redirect::Policy::none())
            .no_proxy()
            .retry(oauth2::reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ApiError::ClientConfiguration)?;
        Ok(Self { manager, client })
    }

    fn post(
        client: &oauth2::reqwest::blocking::Client,
        token: &str,
        body: &[u8],
    ) -> Result<Value, ApiError> {
        use oauth2::reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
        use std::io::Read;

        let mut authorization = Zeroizing::new(Vec::with_capacity(7 + token.len()));
        authorization.extend_from_slice(b"Bearer ");
        authorization.extend_from_slice(token.as_bytes());
        let mut authorization =
            HeaderValue::from_bytes(&authorization).map_err(|_| ApiError::Configuration)?;
        authorization.set_sensitive(true);
        let response = client
            .post(GRAPHQL_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, authorization)
            .body(body.to_vec())
            .send()
            .map_err(|_| ApiError::RequestFailed)?;
        let status = response.status().as_u16();
        let json_content = json_content_type(
            response
                .headers()
                .get_all(CONTENT_TYPE)
                .iter()
                .filter_map(|v| v.to_str().ok()),
        );
        let mut bytes = Vec::with_capacity(4096);
        response
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| ApiError::RequestFailed)?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(ApiError::ResponseTooLarge);
        }
        decode(status, json_content, &bytes)
    }
}

impl Transport for HttpsTransport {
    fn execute(
        &mut self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> Result<Value, ApiError> {
        let body = serde_json::to_vec(
            &json!({ "operationName": operation, "query": query, "variables": variables }),
        )
        .map_err(|_| ApiError::Configuration)?;
        let client = &self.client;
        if write {
            self.manager
                .with_bound_access_token(|token| Self::post(client, token, &body))
        } else {
            self.manager
                .with_verified_read(|token| verified(Self::post(client, token, &body)?))
        }
    }
}

/// Exactly one `application/json` content type, parameters allowed.
fn json_content_type<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
}

/// A GraphQL response's `data`, or the first error it reports.
pub fn decode(status: u16, json_content: bool, body: &[u8]) -> Result<Value, ApiError> {
    if !json_content {
        return Err(if status == 200 {
            ApiError::ContentType
        } else {
            ApiError::HttpStatus(status)
        });
    }
    let reply: Value = serde_json::from_slice(body).map_err(|_| {
        if status == 200 {
            ApiError::ReadFieldsInvalid
        } else {
            ApiError::HttpStatus(status)
        }
    })?;
    if let Some(error) = reply["errors"].as_array().and_then(|errors| errors.first()) {
        let message = error["extensions"]["userPresentableMessage"]
            .as_str()
            .or_else(|| error["message"].as_str())
            .unwrap_or("unknown error");
        return Err(ApiError::Graphql(
            message
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_ERROR_MESSAGE_CHARS)
                .collect(),
        ));
    }
    if status != 200 {
        return Err(ApiError::HttpStatus(status));
    }
    match reply.get("data") {
        Some(data) if data.is_object() => Ok(data.clone()),
        _ => Err(ApiError::ReadFieldsInvalid),
    }
}

/// Accepts a read's data only when its viewer is this plugin's app actor.
pub(crate) fn verified(data: Value) -> Result<VerifiedReadOutcome<Value>, ApiError> {
    let viewer = &data["viewer"];
    if viewer["app"].as_bool() != Some(true) || viewer["isMe"].as_bool() != Some(true) {
        return Err(ApiError::ActorIdentityMismatch);
    }
    let id = viewer["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty() && id.len() <= 4096)
        .ok_or(ApiError::ActorIdentityMismatch)?;
    Ok(VerifiedReadOutcome::with_value(
        Zeroizing::new(id.to_owned()),
        data.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_prefers_graphql_errors_and_requires_json() {
        assert_eq!(decode(200, true, br#"{"data":{"x":1}}"#).unwrap()["x"], 1);
        assert_eq!(
            decode(400, true, br#"{"errors":[{"message":"Entity not found","extensions":{"code":"INVALID_INPUT"}}]}"#),
            Err(ApiError::Graphql("Entity not found".into()))
        );
        assert_eq!(
            decode(200, true, br#"{"data":null,"errors":[{"message":"x","extensions":{"userPresentableMessage":"Readable"}}]}"#),
            Err(ApiError::Graphql("Readable".into()))
        );
        assert_eq!(
            decode(502, false, b"<html>"),
            Err(ApiError::HttpStatus(502))
        );
        assert_eq!(decode(200, false, b"{}"), Err(ApiError::ContentType));
        assert_eq!(decode(200, true, b"{}"), Err(ApiError::ReadFieldsInvalid));
        assert_eq!(decode(429, true, b"{}"), Err(ApiError::HttpStatus(429)));
        assert!(json_content_type(
            ["application/json; charset=utf-8"].into_iter()
        ));
        assert!(!json_content_type(
            ["application/json", "text/html"].into_iter()
        ));
        assert!(!json_content_type(std::iter::empty()));
    }

    #[test]
    fn only_an_app_viewer_is_verified() {
        let data = json!({"viewer":{"id":"app-1","app":true,"isMe":true},"x":1});
        let (viewer, value) = verified(data).unwrap().into_parts();
        assert_eq!(viewer.as_str(), "app-1");
        assert_eq!(value["x"], 1);
        assert_eq!(
            verified(json!({"viewer":{"id":"u","app":false,"isMe":true}})).unwrap_err(),
            ApiError::ActorIdentityMismatch
        );
        assert_eq!(
            verified(json!({})).unwrap_err(),
            ApiError::ActorIdentityMismatch
        );
    }
}
