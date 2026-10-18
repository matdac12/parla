//! The Traccia REST client (PARLA-82, ADR-0004): Mattia's own To-do service,
//! reached over his tailnet with a `todos`-scoped token. Command mode only;
//! nothing here runs at startup or on the dictation path (ADR-0001).

use super::when::{Date, When};
use crate::{provider::openai, settings::Settings};
use reqwest::{
    blocking::RequestBuilder,
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
};
use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, TcpStream},
    sync::{mpsc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, Instant},
};

/// For `/v1/me` and the Projects list, once connected.
const GET_TIMEOUT: Duration = Duration::from_secs(2);
/// For creating a To-do (spec §5).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

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
        // Request paths are appended, so no query or fragment.
        Ok(url)
            if matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.query().is_none()
                && url.fragment().is_none() =>
        {
            Ok(trimmed.into())
        }
        _ => Err(Error::BadUrl),
    }
}

/// Checked by `save_settings` (PARLA-85), only for what this save changes, so
/// a section already on disk never blocks an unrelated save: an edited URL is
/// normalised, and reminders stay on only with a URL and a token.
pub fn validate_settings(settings: &mut Settings, previous: &Settings) -> Result<(), String> {
    let edited = settings.providers.get("traccia") != previous.providers.get("traccia");
    if let Some(serde_json::Value::String(url)) = settings
        .providers
        .get_mut("traccia")
        .and_then(|section| section.get_mut("base_url"))
        .filter(|_| edited)
    {
        if !url.trim().is_empty() {
            *url = normalize_base_url(url).map_err(|e| e.to_string())?;
        }
    }
    if settings.command.reminders && (edited || !previous.command.reminders) {
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
    /// A 3xx: never followed, since a redirect to another origin drops the
    /// token and would read as a token error (an http URL on an https
    /// server).
    Redirect,
    /// Something answers at the address, but the connection failed while
    /// opening: an https URL on a plain HTTP port, or a certificate refused.
    Protocol,
    /// The connection broke once open, before an answer could be read: an
    /// http URL on a TLS port, or a reset. A POST may have gone through.
    Dropped,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "Add the Traccia URL and token first."),
            Self::BadUrl => write!(
                f,
                "The Traccia URL must start with http:// or https:// and have no ? or #."
            ),
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
            Self::Token => write!(
                f,
                "Traccia rejected the token. Check it in Settings → Command mode."
            ),
            Self::Scope => write!(
                f,
                "This Traccia token can't create To-dos (it needs the todos scope)."
            ),
            Self::Status(status) => write!(f, "Traccia error (HTTP {status})."),
            Self::Unreadable => write!(f, "Traccia's reply could not be read."),
            Self::Redirect => write!(
                f,
                "The Traccia URL redirects elsewhere. Check it in Settings → Command mode (http or https?)."
            ),
            Self::Protocol => write!(
                f,
                "Something answers at the Traccia URL, but not as Traccia. Check it in Settings → Command mode (http or https, and the port)."
            ),
            Self::Dropped => write!(
                f,
                "The connection to Traccia broke before it answered. Check the Traccia URL in Settings → Command mode (http or https, and the port)."
            ),
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
        300..=399 => Error::Redirect,
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Project {
    pub key: String,
    pub name: String,
}

/// One page of `GET /v1/projects`, keeping the active and paused Projects. An
/// item that doesn't read is skipped, not the whole page.
fn parse_projects(body: &str) -> Result<Vec<Project>, Error> {
    #[derive(Deserialize)]
    struct Item {
        #[serde(flatten)]
        project: Project,
        status: String,
    }
    #[derive(Deserialize)]
    struct Page {
        items: Vec<serde_json::Value>,
    }
    let page: Page = serde_json::from_str(body).map_err(|_| Error::Unreadable)?;
    Ok(page
        .items
        .into_iter()
        .filter_map(|item| serde_json::from_value::<Item>(item).ok())
        .filter(|item| matches!(item.status.as_str(), "active" | "paused"))
        .map(|item| item.project)
        .collect())
}

/// The To-do Traccia stored, as `POST /v1/todos` returned it: what the
/// history row (and slice 4's Card) shows, not what Parla asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub id: String,
    /// Opens it in the dashboard.
    pub url: String,
    /// As stored, `@KEY`s included.
    pub title: String,
    /// The Planned day, `YYYY-MM-DD`; None for Anytime.
    pub day: Option<String>,
    /// The Reminder's `HH:MM`; None for no Reminder.
    pub time: Option<String>,
}

impl Created {
    /// `Chiamare Luca · 2026-10-18 15:00`, for the history row.
    pub fn summary(&self) -> String {
        match (&self.day, &self.time) {
            (Some(day), Some(time)) => format!("{} · {day} {time}", self.title),
            (Some(day), None) => format!("{} · {day}", self.title),
            _ => self.title.clone(),
        }
    }
}

fn parse_created(body: &str) -> Result<Created, Error> {
    #[derive(Deserialize)]
    struct Reminder {
        time: String,
    }
    #[derive(Deserialize)]
    struct Todo {
        id: String,
        url: String,
        title: String,
        day: Option<String>,
        reminder: Option<Reminder>,
    }
    let todo: Todo = serde_json::from_str(body).map_err(|_| Error::Unreadable)?;
    Ok(Created {
        id: todo.id,
        url: todo.url,
        title: todo.title,
        day: todo.day,
        time: todo.reminder.map(|reminder| reminder.time),
    })
}

/// `{title, remindAt}`, `{title, day}` or `{title}`; off Windows, the day
/// and time as said, which Traccia reads in its Owner timezone.
fn create_body(title: &str, when: &When) -> serde_json::Value {
    match when {
        When::At { utc: Some(utc), .. } => serde_json::json!({"title": title, "remindAt": utc}),
        When::At {
            day,
            time,
            utc: None,
        } => serde_json::json!({
            "title": title,
            "day": day.to_string(),
            "time": time.to_string(),
        }),
        When::Day(day) => serde_json::json!({"title": title, "day": day.to_string()}),
        When::Anytime => serde_json::json!({"title": title}),
    }
}

/// The spec §5 wording for a `create` that failed. A refused moment is one
/// already past (Parla sends nothing malformed), named by the day and time
/// Parla asked for.
pub fn reminder_message(error: &Error, when: &When, today: Date) -> String {
    match (error, when) {
        (Error::Refused { fields, .. }, When::At { day, time, .. })
            if fields
                .iter()
                .any(|field| matches!(field.as_str(), "remindAt" | "time" | "day")) =>
        {
            let day = if *day == today {
                "today".to_string()
            } else {
                format!("on {} {:02}/{:02}", &day.weekday()[..3], day.day, day.month)
            };
            format!("{time} {day} has already passed.")
        }
        (Error::Refused { fields, .. }, _) if fields.is_empty() => {
            "Traccia refused the reminder.".into()
        }
        (Error::Refused { fields, .. }, _) => {
            format!("Traccia refused the reminder ({}).", fields.join(", "))
        }
        // The POST may have gone through: say so rather than invite a duplicate.
        (Error::Dropped | Error::Timeout(_), _) => {
            format!("{error} The reminder may have been created.")
        }
        (error, _) => error.to_string(),
    }
}

/// The shared client's settings, but never following a redirect (see
/// `Error::Redirect`): reqwest sets that per client. Built at the first
/// Traccia call, in command mode only.
fn http() -> Result<&'static reqwest::blocking::Client, Error> {
    static CLIENT: OnceLock<Option<reqwest::blocking::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            openai::client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| eprintln!("Cannot build the Traccia client: {error}"))
                .ok()
        })
        .as_ref()
        .ok_or(Error::Unreachable)
}

/// The real `Reminders` port: the saved connection (or the env overrides),
/// resolved at each run.
pub struct Remote<'a>(pub &'a Settings);

impl super::Reminders for Remote<'_> {
    fn create(&self, title: &str, when: &When) -> Result<Created, Error> {
        let config = TracciaConfig::resolved(self.0)?;
        Client::for_writes(&config).create(title, when)
    }

    /// What the hold's refresh cached, waiting only for a first fetch.
    fn projects(&self) -> Vec<Project> {
        PROJECTS.snapshot(PROJECTS_WAIT)
    }
}

/// Blocking calls: run them off the UI thread.
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

    /// With the POST's budget.
    pub fn for_writes(config: &'a TracciaConfig) -> Self {
        Self {
            config,
            timeout: WRITE_TIMEOUT,
        }
    }

    /// `POST /v1/todos`. Never retried (ADR-0004).
    pub fn create(&self, title: &str, when: &When) -> Result<Created, Error> {
        let request = http()?
            .post(self.url("/v1/todos"))
            .header(CONTENT_TYPE, "application/json")
            .body(create_body(title, when).to_string());
        parse_created(&self.send(request)?)
    }

    /// `DELETE /v1/todos/{id}`, for the live tests until slice 4's Undo.
    #[cfg(test)]
    pub(crate) fn delete(&self, id: &str) -> Result<(), Error> {
        self.send(http()?.delete(self.url(&format!("/v1/todos/{id}"))))
            .map(drop)
    }

    /// The open To-dos' titles (the first 250), for the live tests.
    #[cfg(test)]
    pub(crate) fn open_titles(&self) -> Result<Vec<String>, Error> {
        #[derive(Deserialize)]
        struct Item {
            title: String,
        }
        #[derive(Deserialize)]
        struct Page {
            items: Vec<Item>,
        }
        let page: Page = serde_json::from_str(&self.get("/v1/todos?state=open&limit=250")?)
            .map_err(|_| Error::Unreadable)?;
        Ok(page.items.into_iter().map(|item| item.title).collect())
    }

    pub fn me(&self) -> Result<Me, Error> {
        parse_me(&self.get("/v1/me")?)
    }

    /// The first page (up to 250, Traccia's maximum).
    pub fn projects(&self) -> Result<Vec<Project>, Error> {
        parse_projects(&self.get("/v1/projects?limit=250")?)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.base_url)
    }

    fn get(&self, path: &str) -> Result<String, Error> {
        self.send(http()?.get(self.url(path)))
    }

    /// The body of a 2xx, or the error.
    fn send(&self, request: RequestBuilder) -> Result<String, Error> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", self.config.token))
            .map_err(|_| Error::Token)?;
        authorization.set_sensitive(true);
        let response = request
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

    /// A failure alone can't tell a host that is down from a server that
    /// hangs (the request timeout also covers connecting), nor from one that
    /// speaks another protocol (a TLS handshake fails as a connect error). A
    /// plain TCP connect, within the rest of the shared connect timeout,
    /// tells them apart; a connect that timed out already needs none.
    fn transport(&self, error: &reqwest::Error) -> Error {
        if error.is_connect() && error.is_timeout() || !self.reachable() {
            Error::Unreachable
        } else if error.is_timeout() {
            Error::Timeout(self.timeout)
        } else if error.is_connect() {
            Error::Protocol
        } else {
            Error::Dropped
        }
    }

    fn reachable(&self) -> bool {
        let budget = openai::CONNECT_TIMEOUT
            .saturating_sub(self.timeout)
            .max(Duration::from_secs(1));
        let deadline = Instant::now() + budget;
        let url = reqwest::Url::parse(&self.config.base_url).ok();
        let addresses = resolve_within(budget, move || {
            url.and_then(|url| url.socket_addrs(|| None).ok())
                .unwrap_or_default()
        });
        // One budget for all the addresses (IPv4 and IPv6), not one each.
        addresses.iter().any(|address| {
            let left = deadline.saturating_duration_since(Instant::now());
            !left.is_zero() && TcpStream::connect_timeout(address, left).is_ok()
        })
    }
}

/// `resolve` on a thread of its own, given up after `budget`: a DNS lookup has
/// no deadline of its own, and one that hangs must not outlast the probe.
fn resolve_within(
    budget: Duration,
    resolve: impl FnOnce() -> Vec<SocketAddr> + Send + 'static,
) -> Vec<SocketAddr> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(resolve());
    });
    receiver.recv_timeout(budget).unwrap_or_default()
}

/// A Projects list older than this is fetched again at the next hold.
const PROJECTS_TTL: Duration = Duration::from_secs(10 * 60);
/// The longest a run waits for the first list, only while it is in flight.
const PROJECTS_WAIT: Duration = Duration::from_millis(500);

struct CacheState {
    /// The last list fetched, and when its fetch started.
    list: Option<(Instant, Vec<Project>)>,
    in_flight: bool,
    /// A fetch has landed, list or not: only the first one is waited for.
    tried: bool,
}

/// The Projects list for `@KEY` references (spec §3), in memory only:
/// fetched in the background at the confirmed command hold, so a run reads it
/// without waiting on Traccia. A failed fetch keeps the last list.
pub(crate) struct ProjectsCache {
    state: Mutex<CacheState>,
    landed: Condvar,
}

impl ProjectsCache {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(CacheState {
                list: None,
                in_flight: false,
                tried: false,
            }),
            landed: Condvar::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts `fetch` on a thread of its own when, at `now`, there is no list
    /// or it is older than [`PROJECTS_TTL`], and no fetch is in flight.
    pub fn refresh_if_stale(
        &'static self,
        now: Instant,
        fetch: impl FnOnce() -> Result<Vec<Project>, Error> + Send + 'static,
    ) {
        {
            let mut state = self.state();
            let fresh = state
                .list
                .as_ref()
                .is_some_and(|(at, _)| now.saturating_duration_since(*at) < PROJECTS_TTL);
            if fresh || state.in_flight {
                return;
            }
            state.in_flight = true;
        }
        let land = move |fetched: Result<Vec<Project>, Error>| {
            let mut state = self.state();
            if let Ok(list) = fetched {
                state.list = Some((now, list));
            }
            state.in_flight = false;
            state.tried = true;
            self.landed.notify_all();
        };
        let spawned = std::thread::Builder::new()
            .name("parla-traccia-projects".into())
            .spawn(move || land(fetch()));
        if spawned.is_err() {
            land(Err(Error::Unreachable));
        }
    }

    /// The cached list, even a stale one. Only while the first fetch is in
    /// flight does it wait for it, up to `wait`: once one has failed, runs
    /// go on without a list rather than wait on a Traccia that is down.
    pub fn snapshot(&self, wait: Duration) -> Vec<Project> {
        let (state, _) = self
            .landed
            .wait_timeout_while(self.state(), wait, |state| !state.tried && state.in_flight)
            .unwrap_or_else(PoisonError::into_inner);
        state
            .list
            .as_ref()
            .map(|(_, list)| list.clone())
            .unwrap_or_default()
    }
}

static PROJECTS: ProjectsCache = ProjectsCache::new();

/// At the confirmed command hold (`flow.rs`), next to the LLM pre-warm: the
/// Projects list is fetched in the background when stale, only while
/// reminders are on. Never at startup, never for a dictation.
pub fn refresh_projects(settings: &Settings) {
    refresh_projects_with(
        &PROJECTS,
        settings,
        Instant::now(),
        TracciaConfig::resolved,
        |config| Client::new(&config).projects(),
    );
}

fn refresh_projects_with(
    cache: &'static ProjectsCache,
    settings: &Settings,
    now: Instant,
    resolve: impl FnOnce(&Settings) -> Result<TracciaConfig, Error>,
    fetch: impl FnOnce(TracciaConfig) -> Result<Vec<Project>, Error> + Send + 'static,
) {
    if !settings.command.reminders {
        return;
    }
    // Unconfigured (only through the env overrides): nothing to fetch from.
    if let Ok(config) = resolve(settings) {
        cache.refresh_if_stale(now, move || fetch(config));
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
    use crate::command_mode::when::test_support::{date, time};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
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
            "https://traccia.example.ts.net/?tab=1",
            "https://traccia.example.ts.net/#/projects",
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
    fn a_malformed_project_is_skipped_not_the_whole_page() {
        let body = r#"{"items":[
            {"key":"CRM","name":"CRM","status":"active"},
            {"name":"No key","status":"active"},
            {"key":"NUM","name":42,"status":"active"},
            {"key":"NOS","name":"No status"},
            "not an object",
            {"key":"CIT","name":"Cittadellese","status":"paused"}
        ],"nextCursor":null}"#;
        let keys: Vec<_> = parse_projects(body)
            .unwrap()
            .into_iter()
            .map(|p| p.key)
            .collect();
        assert_eq!(keys, ["CRM", "CIT"]);
    }

    #[test]
    fn the_reachability_lookup_gives_up_at_its_deadline() {
        let address: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert_eq!(
            resolve_within(Duration::from_secs(1), move || vec![address]),
            [address]
        );
        // A lookup that hangs (DNS with Tailscale half up) stops at the budget.
        let started = Instant::now();
        let hung = resolve_within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(3));
            vec![]
        });
        assert!(hung.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
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
                "Traccia rejected the token. Check it in Settings → Command mode.",
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
                let head = String::from_utf8_lossy(&head).into_owned();
                // A POST's body too, or answering early resets the request.
                let length = head
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
                    .unwrap_or(0);
                stream.read_exact(&mut vec![0; length]).unwrap();
                let _ = sender.send(head);
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
    fn at(day: &str, hour: &str, utc: Option<&str>) -> When {
        When::At {
            day: date(day),
            time: time(hour),
            utc: utc.map(Into::into),
        }
    }

    #[test]
    fn the_create_body_carries_a_moment_a_day_or_neither() {
        let title = "Chiamare Luca";
        for (when, body) in [
            (
                at("2026-10-18", "15:00", Some("2026-10-18T13:00:00Z")),
                serde_json::json!({"title": title, "remindAt": "2026-10-18T13:00:00Z"}),
            ),
            // Off Windows: the day and time as said, in Traccia's zone.
            (
                at("2026-10-18", "15:00", None),
                serde_json::json!({"title": title, "day": "2026-10-18", "time": "15:00"}),
            ),
            (
                When::Day(date("2026-10-17")),
                serde_json::json!({"title": title, "day": "2026-10-17"}),
            ),
            (When::Anytime, serde_json::json!({"title": title})),
        ] {
            assert_eq!(create_body(title, &when), body, "{when:?}");
        }
    }

    #[test]
    fn a_created_to_do_reads_its_id_url_title_day_and_time() {
        let created = parse_created(&fixture("todo_created")).unwrap();
        assert_eq!(
            created,
            Created {
                id: "01K8TODO0000000000000000AB".into(),
                url: "https://traccia.example.ts.net/personal?todo=01K8TODO0000000000000000AB"
                    .into(),
                title: "Chiamare Luca @CRM".into(),
                day: Some("2026-10-18".into()),
                time: Some("15:00".into()),
            }
        );
        assert_eq!(created.summary(), "Chiamare Luca @CRM · 2026-10-18 15:00");
        let anytime = parse_created(&fixture("todo_anytime")).unwrap();
        assert_eq!(
            (anytime.day.as_deref(), anytime.time.as_deref()),
            (None, None)
        );
        assert_eq!(anytime.summary(), "Comprare il latte");
        let day_only = Created {
            time: None,
            ..created
        };
        assert_eq!(day_only.summary(), "Chiamare Luca @CRM · 2026-10-18");
        assert_eq!(parse_created(r#"{"id":"x"}"#), Err(Error::Unreadable));
    }

    fn passed_error() -> Error {
        parse_error(400, &fixture("error_400_remind_at"))
    }

    #[test]
    fn a_refused_reminder_says_why_in_the_spec_wording() {
        let today = date("2026-10-17");
        let passed = |when: &When| reminder_message(&passed_error(), when, today);
        assert_eq!(
            passed(&at("2026-10-17", "15:00", Some("2026-10-17T13:00:00Z"))),
            "15:00 today has already passed."
        );
        assert_eq!(
            passed(&at("2026-10-16", "10:00", None)),
            "10:00 on Fri 16/10 has already passed."
        );
        // A moment field with no moment sent is no past moment.
        assert_eq!(
            passed(&When::Day(today)),
            "Traccia refused the reminder (remindAt)."
        );
        let title = parse_error(400, &fixture("error_400_title"));
        assert_eq!(
            reminder_message(&title, &When::Anytime, today),
            "Traccia refused the reminder (title)."
        );
        assert_eq!(
            reminder_message(&parse_error(400, ""), &When::Anytime, today),
            "Traccia refused the reminder."
        );
        // Every other failure as for Test connection (spec §5).
        for error in [
            Error::Unreachable,
            Error::Token,
            Error::Scope,
            Error::Status(502),
            Error::Redirect,
            Error::Protocol,
        ] {
            assert_eq!(
                reminder_message(&error, &When::Anytime, today),
                error.to_string()
            );
        }
        // The POST may have gone through: no invitation to a duplicate.
        assert_eq!(
            reminder_message(&Error::Timeout(WRITE_TIMEOUT), &When::Anytime, today),
            "Traccia didn't answer in 5 s. The reminder may have been created."
        );
        for name in ["error_400_remind_at", "error_400_title"] {
            let text = reminder_message(&parse_error(400, &fixture(name)), &When::Anytime, today);
            assert!(!text.contains("SERVER-MESSAGE"), "{text}");
        }
    }

    #[test]
    fn create_posts_the_to_do_with_the_bearer_token_and_reads_the_201() {
        let (base, heads) = serve(vec![http("201 Created", &fixture("todo_created"))]);
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        let created = Client::for_writes(&config)
            .create(
                "Chiamare Luca @CRM",
                &at("2026-10-18", "15:00", Some("2026-10-18T13:00:00Z")),
            )
            .unwrap();
        assert_eq!(created.id, "01K8TODO0000000000000000AB");
        let head = heads.recv().unwrap();
        assert!(head.starts_with("POST /v1/todos HTTP/1.1\r\n"), "{head}");
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("authorization: bearer test-token\r\n"),
            "{head}"
        );
        assert!(
            lower.contains("content-type: application/json\r\n"),
            "{head}"
        );
        assert_eq!(Client::for_writes(&config).timeout, WRITE_TIMEOUT);
    }

    #[test]
    fn create_maps_a_refusal_and_a_server_that_hangs() {
        let (base, _heads) = serve(vec![http(
            "400 Bad Request",
            &fixture("error_400_remind_at"),
        )]);
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        let refused = Client::for_writes(&config).create("Chiamare", &When::Anytime);
        assert_eq!(refused.err(), Some(passed_error()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        let client = Client {
            timeout: Duration::from_millis(300),
            ..Client::for_writes(&config)
        };
        assert_eq!(
            client.create("Chiamare", &When::Anytime),
            Err(Error::Timeout(Duration::from_millis(300)))
        );
        drop(listener);
    }

    #[test]
    fn a_redirect_is_never_followed_and_says_to_check_the_url() {
        // An http URL on a server that moved to https: following it would
        // drop the token and read as a token error.
        let (base, heads) = serve(vec![
            "HTTP/1.1 301 Moved Permanently\r\nLocation: https://traccia.example.ts.net/v1/me\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let config = TracciaConfig::new(&base, TOKEN).unwrap();
        assert_eq!(Client::new(&config).me(), Err(Error::Redirect));
        heads.recv().unwrap();
        assert!(heads.try_recv().is_err(), "one request only");
        let message = Error::Redirect.to_string();
        assert!(message.contains("Traccia URL"), "{message}");
    }

    /// Writes `reply` as soon as a connection opens, without reading, then
    /// keeps listening for the reachability check.
    fn answer_at_once(reply: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Some(Ok(mut stream)) = listener.incoming().next() {
                let _ = stream.write_all(reply);
                std::thread::sleep(Duration::from_millis(200));
            }
            std::thread::sleep(Duration::from_secs(3));
        });
        address.to_string()
    }

    #[test]
    fn https_to_a_plain_http_port_or_the_reverse_says_to_check_the_url() {
        // https:// on a plain HTTP server: the TLS handshake reads HTTP.
        let plain = answer_at_once(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        let config = TracciaConfig::new(&format!("https://{plain}"), TOKEN).unwrap();
        assert_eq!(Client::new(&config).me(), Err(Error::Protocol));
        // http:// on a TLS port: the server sends a TLS alert, not HTTP.
        let tls = answer_at_once(b"\x15\x03\x03\x00\x02\x02\x46");
        let config = TracciaConfig::new(&format!("http://{tls}"), TOKEN).unwrap();
        assert_eq!(Client::new(&config).me(), Err(Error::Dropped));
        // A POST that broke may have gone through: the message says so.
        let message = reminder_message(&Error::Dropped, &When::Anytime, date("2026-10-17"));
        assert!(message.contains("http or https"), "{message}");
        assert!(
            message.ends_with("The reminder may have been created."),
            "{message}"
        );
        let message = Error::Protocol.to_string();
        assert!(message.contains("http or https"), "{message}");
    }

    // The Projects cache (spec §3), on a fake clock (the `now` passed in) and
    // fake fetchers.

    fn project(key: &str, name: &str) -> Project {
        Project {
            key: key.into(),
            name: name.into(),
        }
    }

    type Calls = std::sync::Arc<AtomicUsize>;

    fn cache() -> &'static ProjectsCache {
        Box::leak(Box::new(ProjectsCache::new()))
    }

    /// Waits for the background fetch to land.
    fn settle(cache: &ProjectsCache) {
        let (state, _) = cache
            .landed
            .wait_timeout_while(cache.state(), Duration::from_secs(5), |state| {
                state.in_flight
            })
            .unwrap();
        assert!(!state.in_flight, "never settled");
    }

    /// A fetcher that counts its calls and answers `result`.
    fn counting(
        calls: &Calls,
        result: Result<Vec<Project>, Error>,
    ) -> impl FnOnce() -> Result<Vec<Project>, Error> + Send + 'static {
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            result
        }
    }

    #[test]
    fn a_missing_or_stale_list_is_fetched_and_a_fresh_one_is_not() {
        let cache = cache();
        let calls = Calls::default();
        let t0 = Instant::now();
        cache.refresh_if_stale(t0, counting(&calls, Ok(vec![project("CRM", "CRM")])));
        settle(cache);
        assert_eq!(cache.snapshot(Duration::ZERO), [project("CRM", "CRM")]);
        // Fresh for 10 minutes.
        cache.refresh_if_stale(
            t0 + Duration::from_secs(9 * 60),
            counting(&calls, Ok(vec![])),
        );
        settle(cache);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Then stale: fetched again, and the new list replaces the old.
        let t1 = t0 + PROJECTS_TTL + Duration::from_secs(1);
        let cit = vec![project("CIT", "Cittadellese")];
        cache.refresh_if_stale(t1, counting(&calls, Ok(cit.clone())));
        settle(cache);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.snapshot(Duration::ZERO), cit);
    }

    #[test]
    fn a_fetch_in_flight_is_not_started_twice() {
        let cache = cache();
        let calls = Calls::default();
        let (release, gate) = mpsc::channel::<()>();
        let first = calls.clone();
        let t0 = Instant::now();
        cache.refresh_if_stale(t0, move || {
            first.fetch_add(1, Ordering::SeqCst);
            let _ = gate.recv();
            Ok(vec![project("CRM", "CRM")])
        });
        // A second hold while the first fetch hangs.
        cache.refresh_if_stale(t0 + Duration::from_secs(1), counting(&calls, Ok(vec![])));
        release.send(()).unwrap();
        settle(cache);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.snapshot(Duration::ZERO), [project("CRM", "CRM")]);
    }

    #[test]
    fn an_empty_cache_waits_at_most_the_bound_and_only_for_a_fetch_in_flight() {
        let cache = cache();
        // Nothing cached, nothing in flight: no wait at all.
        let started = Instant::now();
        assert!(cache.snapshot(Duration::from_secs(5)).is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
        // A fetch that hangs: the run goes on without a list at the bound.
        let (release, gate) = mpsc::channel::<()>();
        cache.refresh_if_stale(Instant::now(), move || {
            let _ = gate.recv();
            Ok(vec![project("CRM", "CRM")])
        });
        let started = Instant::now();
        assert!(cache.snapshot(Duration::from_millis(150)).is_empty());
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(150) && waited < Duration::from_secs(3),
            "{waited:?}"
        );
        // A fetch that lands within the bound is used at once.
        let waiter = std::thread::spawn(move || {
            let started = Instant::now();
            (cache.snapshot(Duration::from_secs(3)), started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        release.send(()).unwrap();
        let (list, waited) = waiter.join().unwrap();
        assert_eq!(list, [project("CRM", "CRM")]);
        assert!(waited < Duration::from_secs(1), "{waited:?}");
        // A list in hand, even stale, never waits for the next fetch.
        let (_hold, gate) = mpsc::channel::<()>();
        cache.refresh_if_stale(Instant::now() + PROJECTS_TTL * 2, move || {
            let _ = gate.recv();
            Ok(vec![])
        });
        let started = Instant::now();
        assert_eq!(
            cache.snapshot(Duration::from_secs(5)),
            [project("CRM", "CRM")]
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn once_a_first_fetch_failed_no_run_waits_again() {
        // Traccia down since startup: every hold retries, but a run only
        // ever waited for the first fetch.
        let cache = cache();
        let calls = Calls::default();
        let t0 = Instant::now();
        cache.refresh_if_stale(t0, counting(&calls, Err(Error::Unreachable)));
        settle(cache);
        let (_hold, gate) = mpsc::channel::<()>();
        cache.refresh_if_stale(t0 + Duration::from_secs(5), move || {
            let _ = gate.recv();
            Ok(vec![])
        });
        let started = Instant::now();
        assert!(cache.snapshot(Duration::from_secs(5)).is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_failed_fetch_keeps_the_last_list_and_is_retried_at_the_next_hold() {
        let cache = cache();
        let calls = Calls::default();
        let t0 = Instant::now();
        let list = vec![project("CRM", "CRM"), project("CIT", "Cittadellese")];
        cache.refresh_if_stale(t0, counting(&calls, Ok(list.clone())));
        settle(cache);
        let t1 = t0 + PROJECTS_TTL + Duration::from_secs(1);
        cache.refresh_if_stale(t1, counting(&calls, Err(Error::Unreachable)));
        settle(cache);
        assert_eq!(cache.snapshot(Duration::ZERO), list);
        // Still stale: the next hold tries again.
        cache.refresh_if_stale(t1 + Duration::from_secs(5), counting(&calls, Ok(vec![])));
        settle(cache);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn the_refresh_never_runs_while_reminders_are_off_or_unconfigured() {
        // The saved section only: TRACCIA_URL or TRACCIA_TOKEN in the
        // developer's shell must not reach this test.
        let saved = |settings: &Settings| TracciaConfig::with_overrides(settings, None, None);
        let mut settings = Settings::default();
        let cache = cache();
        let calls = Calls::default();
        let fetch = |calls: &Calls| {
            let fetch = counting(calls, Ok(vec![project("CRM", "CRM")]));
            move |config: TracciaConfig| {
                assert_eq!(config.token, TOKEN);
                fetch()
            }
        };
        // On, but with no URL or token: nothing starts.
        settings.command.reminders = true;
        refresh_projects_with(cache, &settings, Instant::now(), saved, fetch(&calls));
        assert!(!cache.state().in_flight);
        settings.providers.insert(
            "traccia".into(),
            serde_json::json!({"base_url": URL, "token": TOKEN}),
        );
        settings.command.reminders = false;
        refresh_projects_with(cache, &settings, Instant::now(), saved, fetch(&calls));
        assert!(!cache.state().in_flight);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(cache.snapshot(Duration::ZERO).is_empty());
        settings.command.reminders = true;
        refresh_projects_with(cache, &settings, Instant::now(), saved, fetch(&calls));
        settle(cache);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.snapshot(Duration::ZERO), [project("CRM", "CRM")]);
    }
}
