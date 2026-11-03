//! Temporary S3 credentials from `AssumeRoleWithWebIdentity` at a configurable STS endpoint.
//!
//! `aws-creds` sends web-identity exchanges only to `https://sts.amazonaws.com/`. S3-compatible
//! stores (MinIO, SeaweedFS) serve STS themselves, so with an STS endpoint configured the S3 tier
//! makes the exchange here and renews the session before it expires.

use anyhow::{bail, Context, Result};
use s3::creds::{Credentials, Rfc3339OffsetDateTime};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::{form_urlencoded, Url};

/// How long before a session expires it is replaced, so no request is signed with credentials
/// that lapse in flight.
pub const RENEWAL_MARGIN: Duration = Duration::from_secs(5 * 60);

const DEFAULT_SESSION_NAME: &str = "bazel-diff";
const STS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STS_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Exchanges a web identity token (a Kubernetes ServiceAccount token, say) for S3 credentials and
/// reuses them until [`RENEWAL_MARGIN`] before they expire.
pub struct WebIdentitySessions {
    endpoint: Url,
    role_arn: String,
    token_file: PathBuf,
    session_name: String,
    http: attohttpc::Session,
    current: Mutex<Option<Session>>,
}

struct Session {
    credentials: Credentials,
    expires_at: SystemTime,
}

impl WebIdentitySessions {
    /// Reads the role, token file and session name from `AWS_ROLE_ARN`,
    /// `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_ROLE_SESSION_NAME`, as the AWS SDKs do.
    pub fn from_env(endpoint: &str) -> Result<Self> {
        let role_arn =
            std::env::var("AWS_ROLE_ARN").context("an STS endpoint requires AWS_ROLE_ARN")?;
        let token_file = std::env::var_os("AWS_WEB_IDENTITY_TOKEN_FILE")
            .context("an STS endpoint requires AWS_WEB_IDENTITY_TOKEN_FILE")?;
        let session_name = std::env::var("AWS_ROLE_SESSION_NAME")
            .unwrap_or_else(|_| DEFAULT_SESSION_NAME.to_owned());
        Self::new(endpoint, role_arn, token_file.into(), session_name)
    }

    /// Exchanges the token in `token_file` for `role_arn`'s credentials at `endpoint`.
    pub fn new(
        endpoint: &str,
        role_arn: String,
        token_file: PathBuf,
        session_name: String,
    ) -> Result<Self> {
        let endpoint =
            Url::parse(endpoint).with_context(|| format!("invalid STS endpoint {endpoint:?}"))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            bail!("STS endpoint must be http or https: {endpoint}");
        }
        let mut http = attohttpc::Session::new();
        http.connect_timeout(STS_CONNECT_TIMEOUT);
        http.read_timeout(STS_READ_TIMEOUT);
        // The token travels in the request body; following a redirect would replay it to
        // whatever host the `Location` names.
        http.follow_redirects(false);
        Ok(Self {
            endpoint,
            role_arn,
            token_file,
            session_name,
            http,
            current: Mutex::new(None),
        })
    }

    /// Credentials to sign the next request with: the current session's, or a new session's when
    /// the current one expires within [`RENEWAL_MARGIN`]. Concurrent callers wait for a single
    /// exchange rather than each making their own.
    pub fn credentials(&self) -> Result<Credentials> {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(session) = current.as_ref() {
            if !needs_renewal(session.expires_at, SystemTime::now()) {
                return Ok(session.credentials.clone());
            }
        }
        let session = self.assume_role()?;
        let credentials = session.credentials.clone();
        *current = Some(session);
        Ok(credentials)
    }

    /// One `AssumeRoleWithWebIdentity` call. The token file is read every time because kubelet
    /// rotates a projected ServiceAccount token before it expires.
    fn assume_role(&self) -> Result<Session> {
        let token = fs::read_to_string(&self.token_file).with_context(|| {
            format!(
                "failed to read the web identity token {}",
                self.token_file.display()
            )
        })?;
        // A form post rather than query parameters keeps the token out of access logs.
        let form = form_urlencoded::Serializer::new(String::new())
            .append_pair("Action", "AssumeRoleWithWebIdentity")
            .append_pair("Version", "2011-06-15")
            .append_pair("RoleArn", &self.role_arn)
            .append_pair("RoleSessionName", &self.session_name)
            .append_pair("WebIdentityToken", token.trim())
            .finish();
        let response = self
            .http
            .post(self.endpoint.as_str())
            .header(
                attohttpc::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .text(form)
            .send()
            .with_context(|| format!("failed to reach STS at {}", self.endpoint))?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() {
            bail!(
                "STS at {} refused {}: HTTP {} {}",
                self.endpoint,
                self.role_arn,
                status.as_u16(),
                body.trim()
            );
        }
        parse_session(&body)
    }
}

/// Whether a session expiring at `expires_at` is too close to expiry to sign with at `now`.
fn needs_renewal(expires_at: SystemTime, now: SystemTime) -> bool {
    now + RENEWAL_MARGIN >= expires_at
}

/// The part of an `AssumeRoleWithWebIdentity` answer this reads. Other elements, which
/// S3-compatible stores fill in to varying degrees, are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AssumeRoleWithWebIdentityResponse {
    assume_role_with_web_identity_result: AssumeRoleWithWebIdentityResult,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AssumeRoleWithWebIdentityResult {
    credentials: StsCredentials,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StsCredentials {
    access_key_id: String,
    secret_access_key: String,
    #[serde(default)]
    session_token: Option<String>,
    expiration: Rfc3339OffsetDateTime,
}

fn parse_session(body: &str) -> Result<Session> {
    let response: AssumeRoleWithWebIdentityResponse =
        quick_xml::de::from_str(body).context("STS answered without credentials")?;
    let sts = response.assume_role_with_web_identity_result.credentials;
    let expires_after = u64::try_from(sts.expiration.unix_timestamp()).unwrap_or_default();
    Ok(Session {
        // No `expiration`: `rust-s3` checks it before every request and, once it passes, replaces
        // the credentials from the default provider chain, which would send the token to AWS.
        // Renewal is this module's job, ahead of expiry.
        credentials: Credentials {
            access_key: Some(sts.access_key_id),
            secret_key: Some(sts.secret_access_key),
            security_token: None,
            session_token: sts.session_token,
            expiration: None,
        },
        expires_at: UNIX_EPOCH + Duration::from_secs(expires_after),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use tiny_http::{Response, Server, StatusCode};

    struct CapturedExchange {
        method: String,
        url: String,
        content_type: String,
        form: Vec<(String, String)>,
    }

    fn sts_answer(access_key: &str, expiration: &str) -> String {
        format!(
            r#"<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <Credentials>
      <AccessKeyId>{access_key}</AccessKeyId>
      <SecretAccessKey>secret-{access_key}</SecretAccessKey>
      <SessionToken>token-{access_key}</SessionToken>
      <Expiration>{expiration}</Expiration>
    </Credentials>
  </AssumeRoleWithWebIdentityResult>
</AssumeRoleWithWebIdentityResponse>"#
        )
    }

    fn capture(request: &mut tiny_http::Request) -> CapturedExchange {
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        let content_type = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Content-Type"))
            .map(|header| header.value.to_string())
            .unwrap_or_default();
        CapturedExchange {
            method: request.method().to_string(),
            url: request.url().to_owned(),
            content_type,
            form: form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect(),
        }
    }

    /// A local STS that answers each exchange with the next of `responses` and reports what it
    /// was sent.
    fn sts_with_responses(
        responses: Vec<(u16, String)>,
    ) -> (
        String,
        mpsc::Receiver<CapturedExchange>,
        thread::JoinHandle<()>,
    ) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", server.server_addr().to_ip().unwrap());
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for (status, body) in responses {
                let mut request = server.recv().unwrap();
                sender.send(capture(&mut request)).unwrap();
                request
                    .respond(Response::from_string(body).with_status_code(StatusCode(status)))
                    .unwrap();
            }
        });
        (endpoint, receiver, handle)
    }

    fn token_file(token: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, token).unwrap();
        (dir, path)
    }

    fn sessions(endpoint: &str, token_file: PathBuf) -> WebIdentitySessions {
        WebIdentitySessions::new(
            endpoint,
            "arn:aws:iam::role/cache".to_owned(),
            token_file,
            "bazel-diff".to_owned(),
        )
        .unwrap()
    }

    fn form_value<'a>(exchange: &'a CapturedExchange, name: &str) -> &'a str {
        exchange
            .form
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_else(|| panic!("form has no {name}"))
    }

    #[test]
    fn exchanges_the_token_with_a_form_post_to_the_configured_endpoint() {
        let (endpoint, exchanges, handle) =
            sts_with_responses(vec![(200, sts_answer("first", "2999-01-01T00:00:00Z"))]);
        let (_dir, path) = token_file("service-account-token\n");

        let credentials = sessions(&endpoint, path).credentials().unwrap();

        handle.join().unwrap();
        let exchange = exchanges.recv().unwrap();
        assert_eq!(exchange.method, "POST");
        assert_eq!(exchange.url, "/");
        assert_eq!(exchange.content_type, "application/x-www-form-urlencoded");
        assert_eq!(form_value(&exchange, "Action"), "AssumeRoleWithWebIdentity");
        assert_eq!(form_value(&exchange, "Version"), "2011-06-15");
        assert_eq!(form_value(&exchange, "RoleArn"), "arn:aws:iam::role/cache");
        assert_eq!(form_value(&exchange, "RoleSessionName"), "bazel-diff");
        assert_eq!(
            form_value(&exchange, "WebIdentityToken"),
            "service-account-token"
        );
        assert_eq!(credentials.access_key.as_deref(), Some("first"));
        assert_eq!(credentials.secret_key.as_deref(), Some("secret-first"));
        assert_eq!(credentials.session_token.as_deref(), Some("token-first"));
        assert_eq!(credentials.expiration, None);
    }

    #[test]
    fn reuses_a_session_that_is_not_close_to_expiry() {
        let (endpoint, exchanges, handle) =
            sts_with_responses(vec![(200, sts_answer("first", "2999-01-01T00:00:00Z"))]);
        let (_dir, path) = token_file("token");
        let sessions = sessions(&endpoint, path);

        let first = sessions.credentials().unwrap();
        let second = sessions.credentials().unwrap();

        handle.join().unwrap();
        assert_eq!(exchanges.try_iter().count(), 1);
        assert_eq!(first, second);
    }

    #[test]
    fn renews_an_expiring_session_with_the_current_token() {
        let (endpoint, exchanges, handle) = sts_with_responses(vec![
            (200, sts_answer("first", "2000-01-01T00:00:00Z")),
            (200, sts_answer("second", "2999-01-01T00:00:00Z")),
        ]);
        let (_dir, path) = token_file("old-token");
        let sessions = sessions(&endpoint, path.clone());

        sessions.credentials().unwrap();
        fs::write(&path, "rotated-token").unwrap();
        let renewed = sessions.credentials().unwrap();

        handle.join().unwrap();
        let tokens: Vec<String> = exchanges
            .try_iter()
            .map(|exchange| form_value(&exchange, "WebIdentityToken").to_owned())
            .collect();
        assert_eq!(tokens, ["old-token", "rotated-token"]);
        assert_eq!(renewed.access_key.as_deref(), Some("second"));
    }

    #[test]
    fn reports_a_refused_exchange_with_the_status_and_body() {
        let (endpoint, _exchanges, handle) =
            sts_with_responses(vec![(403, "AccessDenied: token expired".to_owned())]);
        let (_dir, path) = token_file("token");

        let error = sessions(&endpoint, path).credentials().unwrap_err();

        handle.join().unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("HTTP 403"), "{message}");
        assert!(message.contains("AccessDenied: token expired"), "{message}");
    }

    #[test]
    fn reports_an_answer_without_credentials() {
        let error = parse_session("<ErrorResponse><Error/></ErrorResponse>").err();

        assert!(error.is_some());
    }

    #[test]
    fn reports_a_missing_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = sessions("http://127.0.0.1:9/", dir.path().join("absent"));

        let error = format!("{:#}", sessions.credentials().unwrap_err());

        assert!(error.contains("web identity token"), "{error}");
    }

    #[test]
    fn rejects_an_endpoint_that_is_not_http() {
        let error = WebIdentitySessions::new(
            "ftp://sts.example",
            "role".to_owned(),
            PathBuf::from("token"),
            "bazel-diff".to_owned(),
        )
        .err();

        assert!(error.is_some());
    }

    #[test]
    fn renews_within_the_margin_and_not_before() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);

        assert!(needs_renewal(now, now));
        assert!(needs_renewal(now + RENEWAL_MARGIN, now));
        assert!(!needs_renewal(
            now + RENEWAL_MARGIN + Duration::from_secs(1),
            now
        ));
    }
}
