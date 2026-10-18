//! The Traccia REST client (PARLA-82, ADR-0004): Mattia's own To-do service,
//! reached over his tailnet with a `todos`-scoped token. Command mode only;
//! nothing here runs at startup or on the dictation path (ADR-0001).

use crate::{provider::openai, settings::Settings};
use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use std::{
    net::TcpStream,
    time::{Duration, Instant},
};

/// For `/v1/me` and the Projects list, once connected.
const GET_TIMEOUT: Duration = Duration::from_secs(2);

/// `providers.traccia`. The env overrides are for the live test only.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TracciaConfig {
    pub base_url: String,
    pub token: String,
}

impl TracciaConfig {
    /// A usable connection: a normalised URL and a token, both set.
    pub fn new(base_url: &str, token: &str) -> Result<Self, Error> {
        let base_url = normalize_base_url(base_url)?;
        let token = token.trim();
        if token.is_empty() {
            return Err(Error::Missing);
        }
        Ok(Self {
            base_url,
            token: token.into(),
        })
    }

    /// The saved section, with `TRACCIA_URL` and `TRACCIA_TOKEN` overriding
    /// each field, resolved at use time like `OPENAI_API_KEY`.
    pub fn resolved(settings: &Settings) -> Result<Self, Error> {
        let env = |name| {
            std::env::var(name)
                .ok()
                .filter(|v: &String| !v.trim().is_empty())
        };
        Self::with_overrides(settings, env("TRACCIA_URL"), env("TRACCIA_TOKEN"))
    }

    fn with_overrides(
        settings: &Settings,
        base_url: Option<String>,
        token: Option<String>,
    ) -> Result<Self, Error> {
        let saved: Self = settings.provider_config("traccia");
        Self::new(
            &base_url.unwrap_or(saved.base_url),
            &token.unwrap_or(saved.token),
        )
    }
}

/// Trimmed, without a trailing slash, and http or https only (the tailnet
/// encrypts either).
pub fn normalize_base_url(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Error::Missing);
    }
    match reqwest::Url::parse(trimmed) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => Ok(trimmed.into()),
        _ => Err(Error::BadUrl),
    }
}

/// Checked when the Settings form saves (PARLA-85): the stored URL is
/// normalised, and reminders turn on only with a URL and a token.
pub fn validate_settings(settings: &mut Settings) -> Result<(), String> {
    if let Some(serde_json::Value::String(url)) = settings
        .providers
        .get_mut("traccia")
        .and_then(|section| section.get_mut("base_url"))
    {
        if !url.trim().is_empty() {
            *url = normalize_base_url(url).map_err(|e| e.to_string())?;
        }
    }
    if settings.command.reminders {
        let saved: TracciaConfig = settings.provider_config("traccia");
        TracciaConfig::new(&saved.base_url, &saved.token).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// What a call failed on. Only Traccia's error `code` and the names in
/// `details.fields` are kept, never its `message` (it may echo input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Missing,
    BadUrl,
    /// DNS failed (Tailscale off), or the connection was refused or timed out.
    Unreachable,
    /// Connected, but no answer within this budget.
    Timeout(Duration),
    /// 400.
    Refused {
        code: Option<String>,
        fields: Vec<String>,
    },
    /// 401.
    Token,
    /// 403.
    Scope,
    /// 5xx, and any other status.
    Status(u16),
    /// A 2xx body that isn't the expected JSON.
    Unreadable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "Add the Traccia URL and token first."),
            Self::BadUrl => write!(f, "The Traccia URL must start with http:// or https://."),
            Self::Unreachable => write!(f, "Traccia can't be reached. Is Tailscale on?"),
            Self::Timeout(budget) => {
                write!(f, "Traccia didn't answer in {} s.", budget.as_secs())
            }
            Self::Refused { fields, .. } if fields.is_empty() => {
                write!(f, "Traccia refused the request.")
            }
            Self::Refused { fields, .. } => {
                write!(f, "Traccia refused the request ({}).", fields.join(", "))
            }
            Self::Token => write!(f, "Traccia rejected the token. Check Settings → Reminders."),
            Self::Scope => write!(
                f,
                "This Traccia token can't create To-dos (it needs the todos scope)."
            ),
            Self::Status(status) => write!(f, "Traccia error (HTTP {status})."),
            Self::Unreadable => write!(f, "Traccia's reply could not be read."),
        }
    }
}

/// A non-2xx answer. The body is `{error:{code,message,details}}`; anything
/// else still maps by status.
fn parse_error(status: u16, body: &str) -> Error {
    match status {
        400 => {
            let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
            let error = &value["error"];
            let code = error["code"].as_str().map(Into::into);
            let mut fields: Vec<String> = error["details"]["fields"]
                .as_object()
                .map(|fields| fields.keys().cloned().collect())
                .unwrap_or_default();
            fields.sort();
            Error::Refused { code, fields }
        }
        401 => Error::Token,
        403 => Error::Scope,
        _ => Error::Status(status),
    }
}

/// `GET /v1/me`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    pub actor: String,
    pub token_name: String,
}

fn parse_me(body: &str) -> Result<Me, Error> {
    serde_json::from_str(body).map_err(|_| Error::Unreadable)
}

/// A Project a reminder can reference as `@KEY`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct Project {
    pub key: String,
    pub name: String,
}

/// One page of `GET /v1/projects`, keeping the active and paused Projects.
fn parse_projects(body: &str) -> Result<Vec<Project>, Error> {
    #[derive(Deserialize)]
    struct Item {
        #[serde(flatten)]
        project: Project,
        status: String,
    }
    #[derive(Deserialize)]
    struct Page {
        items: Vec<Item>,
    }
    let page: Page = serde_json::from_str(body).map_err(|_| Error::Unreadable)?;
    Ok(page
        .items
        .into_iter()
        .filter(|item| matches!(item.status.as_str(), "active" | "paused"))
        .map(|item| item.project)
        .collect())
}

/// Blocking calls on the shared client: run them off the UI thread.
pub struct Client<'a> {
    config: &'a TracciaConfig,
    timeout: Duration,
}

impl<'a> Client<'a> {
    pub fn new(config: &'a TracciaConfig) -> Self {
        Self {
            config,
            timeout: GET_TIMEOUT,
        }
    }

    pub fn me(&self) -> Result<Me, Error> {
        parse_me(&self.get("/v1/me")?)
    }

    /// The first page (up to 250, Traccia's maximum).
    pub fn projects(&self) -> Result<Vec<Project>, Error> {
        parse_projects(&self.get("/v1/projects?limit=250")?)
    }

    fn get(&self, path: &str) -> Result<String, Error> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", self.config.token))
            .map_err(|_| Error::Token)?;
        authorization.set_sensitive(true);
        let client = openai::client().map_err(|_| Error::Unreachable)?;
        let response = client
            .get(format!("{}{path}", self.config.base_url))
            .timeout(self.timeout)
            .header(AUTHORIZATION, authorization)
            .send()
            .map_err(|e| self.transport(&e))?;
        let status = response.status();
        let body = response.text().map_err(|e| self.transport(&e))?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(parse_error(status.as_u16(), &body))
        }
    }

    /// The request timeout also covers connecting, so it can't tell a host
    /// that is down from a server that hangs. Only then, a plain TCP connect
    /// within the rest of the shared connect timeout tells them apart.
    fn transport(&self, error: &reqwest::Error) -> Error {
        if error.is_timeout() && !error.is_connect() && self.reachable() {
            Error::Timeout(self.timeout)
        } else {
            Error::Unreachable
        }
    }

    fn reachable(&self) -> bool {
        let budget = openai::CONNECT_TIMEOUT
            .saturating_sub(self.timeout)
            .max(Duration::from_secs(1));
        let deadline = Instant::now() + budget;
        let addresses = reqwest::Url::parse(&self.config.base_url)
            .ok()
            .and_then(|url| url.socket_addrs(|| None).ok())
            .unwrap_or_default();
        // One budget for all the addresses (IPv4 and IPv6), not one each.
        addresses.iter().any(|address| {
            let left = deadline.saturating_duration_since(Instant::now());
            !left.is_zero() && TcpStream::connect_timeout(address, left).is_ok()
        })
    }
}

/// What Test connection reports.
#[derive(Debug, PartialEq, Eq)]
pub struct Connection {
    pub me: Me,
    pub projects: usize,
}

impl Connection {
    pub fn summary(&self) -> String {
        let plural = if self.projects == 1 { "" } else { "s" };
        format!(
            "Connected as {} · token {} · {} project{plural}",
            self.me.actor, self.me.token_name, self.projects
        )
    }

    /// To-dos made with an agent's token would be stamped as the agent's.
    pub fn warning(&self) -> Option<String> {
        (self.me.actor != "you").then(|| {
            format!(
                "This token writes as {}, so To-dos would be stamped as theirs. Create one with: traccia token create --name parla --actor you --scope todos",
                self.me.actor
            )
        })
    }
}

/// Test connection: `/v1/me`, then the Projects count.
pub fn check(config: &TracciaConfig) -> Result<Connection, Error> {
    let client = Client::new(config);
    let me = client.me()?;
    let projects = client.projects()?.len();
    Ok(Connection { me, projects })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
    };

    const URL: &str = "https://traccia.example.ts.net";
    const TOKEN: &str = "test-token";

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/traccia/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn the_base_url_is_trimmed_without_a_trailing_slash_and_http_or_https_only() {
        for (raw, normalised) in [
            ("https://traccia.example.ts.net", URL),
            ("  https://traccia.example.ts.net/ \n", URL),
            ("https://traccia.example.ts.net//", URL),
            ("http://100.64.0.7:8080/", "http://100.64.0.7:8080"),
            (
                "https://example.ts.net/traccia/",
                "https://example.ts.net/traccia",
            ),
        ] {
            assert_eq!(normalize_base_url(raw).as_deref(), Ok(normalised), "{raw}");
        }
        for raw in [
            "traccia.example.ts.net",
            "ftp://traccia.example.ts.net",
            "https://",
            "file:///C:/traccia",
            "mailto:me@example.com",
        ] {
            assert_eq!(normalize_base_url(raw), Err(Error::BadUrl), "{raw}");
        }
        assert_eq!(normalize_base_url("  "), Err(Error::Missing));
    }

    #[test]
    fn a_config_needs_a_url_and_a_token() {
        let config =
            TracciaConfig::new(" https://traccia.example.ts.net/", " test-token ").unwrap();
        assert_eq!(config.base_url, URL);
        assert_eq!(config.token, TOKEN);
        for (url, token) in [("", TOKEN), (URL, ""), (URL, "   ")] {
            assert_eq!(TracciaConfig::new(url, token).err(), Some(Error::Missing));
        }
        assert_eq!(
            TracciaConfig::new("traccia.example.ts.net", TOKEN).err(),
            Some(Error::BadUrl)
        );
    }

    #[test]
    fn the_env_overrides_each_field_of_the_saved_section() {
        let mut settings = Settings::default();
        settings.providers.insert(
            "traccia".into(),
            serde_json::json!({"base_url": "https://saved.example.ts.net/", "token": "saved-token"}),
        );
        let saved = TracciaConfig::with_overrides(&settings, None, None).unwrap();
        assert_eq!(saved.base_url, "https://saved.example.ts.net");
        assert_eq!(saved.token, "saved-token");
        let overridden =
            TracciaConfig::with_overrides(&settings, Some(URL.into()), Some(TOKEN.into())).unwrap();
        assert_eq!(overridden.base_url, URL);
        assert_eq!(overridden.token, TOKEN);
        assert_eq!(
            TracciaConfig::with_overrides(&Settings::default(), None, None).err(),
            Some(Error::Missing)
        );
    }

    #[test]
    fn me_reads_the_actor_and_the_token_name() {
        assert_eq!(
            parse_me(&fixture("me")),
            Ok(Me {
                actor: "you".into(),
                token_name: "parla".into()
            })
        );
        assert_eq!(parse_me("<html>"), Err(Error::Unreadable));
    }

    #[test]
    fn projects_keep_the_active_and_paused_ones() {
        let projects = parse_projects(&fixture("projects")).unwrap();
        let keys: Vec<_> = projects.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["CRM", "CIT", "PARLA"]);
        assert_eq!(projects[1].name, "Cittadellese");
        assert_eq!(parse_projects(r#"{"items":"no"}"#), Err(Error::Unreadable));
    }

    #[test]
    fn each_error_class_gets_its_message() {
        let cases = [
            (
                400,
                fixture("error_400"),
                "Traccia refused the request (cursor, limit).",
            ),
            (400, String::new(), "Traccia refused the request."),
            (
                401,
                fixture("error_401"),
                "Traccia rejected the token. Check Settings → Reminders.",
            ),
            (
                403,
                fixture("error_403"),
                "This Traccia token can't create To-dos (it needs the todos scope).",
            ),
            (502, fixture("error_502"), "Traccia error (HTTP 502)."),
            (500, "<html>busy</html>".into(), "Traccia error (HTTP 500)."),
            (404, String::new(), "Traccia error (HTTP 404)."),
        ];
        for (status, body, message) in cases {
            assert_eq!(parse_error(status, &body).to_string(), message, "{status}");
        }
        assert_eq!(
            parse_error(400, &fixture("error_400")),
            Error::Refused {
                code: Some("validation_error".into()),
                fields: vec!["cursor".into(), "limit".into()],
            }
        );
        assert_eq!(
            Error::Unreachable.to_string(),
            "Traccia can't be reached. Is Tailscale on?"
        );
        assert_eq!(
            Error::Timeout(GET_TIMEOUT).to_string(),
            "Traccia didn't answer in 2 s."
        );
    }

    #[test]
    fn no_error_text_holds_the_server_message_or_the_token() {
        for (status, name) in [
            (400, "error_400"),
            (401, "error_401"),
            (403, "error_403"),
            (502, "error_502"),
        ] {
            let error = parse_error(status, &fixture(name));
            for text in [error.to_string(), format!("{error:?}")] {
                assert!(!text.contains("SERVER-MESSAGE"), "{text}");
                assert!(!text.contains(TOKEN), "{text}");
            }
        }
    }

    #[test]
    fn the_summary_names_the_actor_the_token_and_the_project_count() {
        let mut connection = Connection {
            me: Me {
                actor: "you".into(),
                token_name: "parla".into(),
            },
            projects: 12,
        };
        assert_eq!(
            connection.summary(),
            "Connected as you · token parla · 12 projects"
        );
        assert_eq!(connection.warning(), None);
        connection.projects = 1;
        assert_eq!(
            connection.summary(),
            "Connected as you · token parla · 1 project"
        );
        connection.me.actor = "agent".into();
        let warning = connection.warning().unwrap();
        assert!(warning.contains("agent"), "{warning}");
        assert!(warning.contains("--actor you"), "{warning}");
    }

    /// Answers each of `responses` on its own connection, then hands back the
    /// request heads it read.
    fn serve(responses: Vec<String>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    head.push(byte[0]);
                }
                let _ = sender.send(String::from_utf8_lossy(&head).into_owned());
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (base, receiver)
    }

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn check_reads_me_then_the_projects_with_the_bearer_token() {
        let (base, heads) = serve(vec![
            http("200 OK", &fixture("me")),
            http("200 OK", &fixture("projects")),
        ]);
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        let connection = check(&config).unwrap();
        assert_eq!(
            connection.summary(),
            "Connected as you · token parla · 3 projects"
        );
        let me = heads.recv().unwrap();
        assert!(me.starts_with("GET /v1/me HTTP/1.1\r\n"), "{me}");
        assert!(
            me.to_ascii_lowercase()
                .contains("authorization: bearer test-token\r\n"),
            "{me}"
        );
        let projects = heads.recv().unwrap();
        assert!(
            projects.starts_with("GET /v1/projects?limit=250 HTTP/1.1\r\n"),
            "{projects}"
        );
    }

    #[test]
    fn an_agent_token_connects_with_a_warning() {
        let (base, _heads) = serve(vec![
            http("200 OK", &fixture("me_agent")),
            http("200 OK", r#"{"items":[],"nextCursor":null}"#),
        ]);
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        let connection = check(&config).unwrap();
        assert_eq!(
            connection.summary(),
            "Connected as agent · token ci-bot · 0 projects"
        );
        assert!(connection.warning().is_some());
    }

    #[test]
    fn check_stops_at_the_first_error() {
        let (base, _heads) = serve(vec![http("401 Unauthorized", &fixture("error_401"))]);
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        assert_eq!(check(&config).err(), Some(Error::Token));
    }

    #[test]
    fn a_closed_port_cannot_be_reached() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        assert_eq!(Client::new(&config).me(), Err(Error::Unreachable));
    }

    #[test]
    fn a_server_that_accepts_but_never_answers_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        let client = Client {
            timeout: std::time::Duration::from_millis(300),
            ..Client::new(&config)
        };
        let started = std::time::Instant::now();
        assert_eq!(
            client.me(),
            Err(Error::Timeout(std::time::Duration::from_millis(300)))
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        drop(listener);
    }
}
