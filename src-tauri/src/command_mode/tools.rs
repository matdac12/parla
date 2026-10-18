//! The tool allowlist (ADR-0002): what command mode's LLM may ask Parla to do
//! instead of replying with text. Each tool is a strict JSON schema, a
//! validator and an executor. Tools join one at a time and never include a
//! shell or anything destructive: `open_url` (PARLA-65), and
//! `create_reminder` (PARLA-86) while reminders are on.

use super::{
    responses::ToolCall,
    when::{Date, Time},
    UrlOpener,
};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};

pub const OPEN_URL: &str = "open_url";
pub const CREATE_REMINDER: &str = "create_reminder";
/// Traccia's limit for a To-do's title.
const MAX_TITLE_CHARS: usize = 500;

/// A tool call that passed the allowlist and its validator, ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// An http or https page, for the default browser.
    OpenUrl(Url),
    /// A To-do in Traccia: a trimmed title, and the local day and time the
    /// model heard, each possibly unsaid.
    CreateReminder {
        title: String,
        day: Option<Date>,
        time: Option<Time>,
    },
}

/// The allowlist and the validators: an unknown tool (`create_reminder`
/// while reminders are off), malformed arguments, an address that is not an
/// http(s) page or a reminder that is not a real day and time is refused,
/// with the reason.
pub fn validate(call: &ToolCall, reminders: bool) -> Result<Tool, String> {
    match call.name.as_str() {
        OPEN_URL => parse_web_page(&call.arguments).map(Tool::OpenUrl),
        CREATE_REMINDER if reminders => parse_reminder(&call.arguments)
            .ok_or_else(|| "the reminder could not be read.".to_string()),
        name => Err(format!("the reply asked for an unknown tool ({name}).")),
    }
}

/// The real `UrlOpener`: the shell's default handler for the page, which is
/// the default browser.
pub struct DefaultBrowser;
impl UrlOpener for DefaultBrowser {
    #[cfg(windows)]
    fn open(&self, url: &str) -> Result<(), String> {
        use windows::{
            core::{w, HSTRING, PCWSTR},
            Win32::{
                System::Com::{
                    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
                    COINIT_DISABLE_OLE1DDE,
                },
                UI::{
                    Shell::{
                        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
                    },
                    WindowsAndMessaging::SW_SHOWNORMAL,
                },
            },
        };
        let file = HSTRING::from(url);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            // Done before returning (the run's thread ends soon), no error box.
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: w!("open"),
            lpFile: PCWSTR(file.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // SAFETY: `info` and `file` outlive the call. The shell may hand the
        // page to a COM handler, so COM is initialised as the docs ask, and
        // uninitialised only if this call initialised it.
        unsafe {
            let com =
                CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok();
            let opened = ShellExecuteExW(&mut info);
            if com {
                CoUninitialize();
            }
            opened.map_err(|error| error.message())
        }
    }

    /// Parla opens pages only on Windows.
    #[cfg(not(windows))]
    fn open(&self, _url: &str) -> Result<(), String> {
        Err("opening a page needs Windows".into())
    }
}

#[derive(Deserialize)]
struct OpenUrlArguments {
    url: String,
}

/// Only http and https pages with a host: never a file, a program, a script
/// or another app's protocol.
fn parse_web_page(arguments: &str) -> Result<Url, String> {
    let arguments: OpenUrlArguments = serde_json::from_str(arguments)
        .map_err(|_| "the reply's open_url call could not be read.".to_string())?;
    let url = Url::parse(arguments.url.trim())
        .map_err(|_| "the reply's address is not a valid web address.".to_string())?;
    // http and https addresses always have a host, or they do not parse.
    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(format!(
            "refused to open a {scheme}: address; only web pages open."
        )),
    }
}

#[derive(Deserialize)]
struct ReminderArguments {
    title: String,
    /// Values, so a missing field is refused rather than read as null.
    day: Value,
    time: Value,
}

/// Every field is required, as in the strict schema; null means unsaid.
fn parse_reminder(arguments: &str) -> Option<Tool> {
    let arguments: ReminderArguments = serde_json::from_str(arguments).ok()?;
    let title = arguments.title.trim();
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        return None;
    }
    let day = match arguments.day {
        Value::Null => None,
        Value::String(day) => Some(Date::parse(&day)?),
        _ => return None,
    };
    let time = match arguments.time {
        Value::Null => None,
        Value::String(time) => Some(Time::parse(&time)?),
        _ => return None,
    };
    Some(Tool::CreateReminder {
        title: title.into(),
        day,
        time,
    })
}

/// The `tools` the request declares, strict so the arguments always match
/// the schema. With reminders off, exactly the list before PARLA-86.
pub fn declarations(reminders: bool) -> Value {
    let mut tools = vec![open_url_declaration()];
    if reminders {
        tools.push(create_reminder_declaration());
    }
    Value::Array(tools)
}

/// The wording held in PARLA-82's probe; `day` and `time` are local.
fn create_reminder_declaration() -> Value {
    json!({
        "type": "function",
        "name": CREATE_REMINDER,
        "description": "Create a to-do with an optional reminder in the user's personal task list.",
        "strict": true,
        "parameters": {
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "What to do."},
                "day": {
                    "type": ["string", "null"],
                    "format": "date",
                    "description": "The planned day, YYYY-MM-DD, or null.",
                },
                "time": {
                    "type": ["string", "null"],
                    "pattern": "^([01][0-9]|2[0-3]):[0-5][0-9]$",
                    "description": "The reminder time, HH:MM, or null.",
                },
            },
            "required": ["title", "day", "time"],
            "additionalProperties": false,
        },
    })
}

fn open_url_declaration() -> Value {
    json!({
        "type": "function",
        "name": OPEN_URL,
        "description": "Open a web page in the user's default browser. For a search, open a search engine's results page.",
        "strict": true,
        "parameters": {
            "type": "object",
            "properties": {"url": {
                "type": "string",
                "description": "The absolute http or https URL to open."
            }},
            "required": ["url"],
            "additionalProperties": false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, url: &str) -> ToolCall {
        ToolCall {
            name: name.into(),
            arguments: json!({ "url": url }).to_string(),
        }
    }

    fn opens(url: &str) -> Result<String, String> {
        match validate(&call(OPEN_URL, url), false) {
            Ok(Tool::OpenUrl(url)) => Ok(url.to_string()),
            Ok(other) => panic!("{other:?}"),
            Err(reason) => Err(reason),
        }
    }

    #[test]
    fn open_url_is_declared_strict_with_one_required_url() {
        assert_eq!(
            declarations(false),
            json!([{
                "type": "function",
                "name": "open_url",
                "description": "Open a web page in the user's default browser. For a search, open a search engine's results page.",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {"url": {
                        "type": "string",
                        "description": "The absolute http or https URL to open."
                    }},
                    "required": ["url"],
                    "additionalProperties": false,
                },
            }])
        );
    }

    #[test]
    fn web_pages_pass_the_validator() {
        assert_eq!(
            opens("https://it.wikipedia.org/wiki/Roma"),
            Ok("https://it.wikipedia.org/wiki/Roma".into())
        );
        assert_eq!(
            opens(" http://example.com/a b?q=ristoranti a Milano "),
            Ok("http://example.com/a%20b?q=ristoranti%20a%20Milano".into())
        );
        let Ok(Tool::OpenUrl(url)) =
            validate(&call(OPEN_URL, "https://www.google.com/search?q=x"), false)
        else {
            panic!("a search page opens")
        };
        assert_eq!(url.host_str(), Some("www.google.com"));
    }

    #[test]
    fn anything_but_an_http_or_https_page_is_refused() {
        for url in [
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)",
            "ms-settings:privacy",
            "mailto:mattia@example.com",
            "ftp://example.com/",
            r"C:\Windows\notepad.exe",
            r"\\server\share",
            "www.google.com",
            "https://",
            "",
        ] {
            assert!(opens(url).is_err(), "{url:?} must not open");
        }
    }

    /// Opens a real tab: `cargo test --lib default_browser -- --ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore = "opens https://example.com in the default browser"]
    fn the_default_browser_opens_a_page() {
        assert_eq!(DefaultBrowser.open("https://example.com/"), Ok(()));
    }

    #[test]
    fn unknown_tools_and_malformed_arguments_are_refused() {
        assert!(validate(&call("run_shell", "https://example.com"), true).is_err());
        for arguments in [
            "",
            "not json",
            r#"{"href":"https://example.com"}"#,
            r#"{"url":7}"#,
        ] {
            let call = ToolCall {
                name: OPEN_URL.into(),
                arguments: arguments.into(),
            };
            assert!(validate(&call, true).is_err(), "{arguments:?}");
        }
    }

    const OPEN_URL_DECLARATION: &str = r#"{"description":"Open a web page in the user's default browser. For a search, open a search engine's results page.","name":"open_url","parameters":{"additionalProperties":false,"properties":{"url":{"description":"The absolute http or https URL to open.","type":"string"}},"required":["url"],"type":"object"},"strict":true,"type":"function"}"#;

    #[test]
    fn create_reminder_is_declared_only_while_reminders_are_on() {
        // Off: byte for byte the request before PARLA-86.
        assert_eq!(
            declarations(false).to_string(),
            format!("[{OPEN_URL_DECLARATION}]")
        );
        assert_eq!(
            declarations(true).to_string(),
            format!(
                "[{OPEN_URL_DECLARATION},{}]",
                r#"{"description":"Create a to-do with an optional reminder in the user's personal task list.","name":"create_reminder","parameters":{"additionalProperties":false,"properties":{"day":{"description":"The planned day, YYYY-MM-DD, or null.","format":"date","type":["string","null"]},"time":{"description":"The reminder time, HH:MM, or null.","pattern":"^([01][0-9]|2[0-3]):[0-5][0-9]$","type":["string","null"]},"title":{"description":"What to do.","type":"string"}},"required":["title","day","time"],"type":"object"},"strict":true,"type":"function"}"#
            )
        );
    }

    fn reminder(arguments: Value) -> ToolCall {
        ToolCall {
            name: CREATE_REMINDER.into(),
            arguments: arguments.to_string(),
        }
    }

    #[test]
    fn a_reminder_call_reads_its_title_day_and_time() {
        assert_eq!(
            validate(
                &reminder(
                    json!({"title": " Chiamare Luca ", "day": "2026-10-18", "time": "15:00"})
                ),
                true
            ),
            Ok(Tool::CreateReminder {
                title: "Chiamare Luca".into(),
                day: Date::parse("2026-10-18"),
                time: Time::parse("15:00"),
            })
        );
        assert_eq!(
            validate(
                &reminder(json!({"title": "Comprare il latte", "day": null, "time": null})),
                true
            ),
            Ok(Tool::CreateReminder {
                title: "Comprare il latte".into(),
                day: None,
                time: None,
            })
        );
        let longest = "a".repeat(500);
        assert!(validate(
            &reminder(json!({"title": longest, "day": null, "time": null})),
            true
        )
        .is_ok());
    }

    #[test]
    fn a_reminder_with_a_bad_title_day_or_time_is_refused() {
        for arguments in [
            json!({"title": "", "day": null, "time": null}),
            json!({"title": "   ", "day": null, "time": null}),
            json!({"title": "a".repeat(501), "day": null, "time": null}),
            json!({"title": "Pagare", "day": "2026-02-30", "time": null}),
            json!({"title": "Pagare", "day": "domani", "time": null}),
            json!({"title": "Pagare", "day": null, "time": "24:00"}),
            json!({"title": "Pagare", "day": null, "time": "3pm"}),
            json!({"title": "Pagare", "day": null}),
            json!({"title": 7, "day": null, "time": null}),
        ] {
            assert_eq!(
                validate(&reminder(arguments.clone()), true),
                Err("the reminder could not be read.".into()),
                "{arguments}"
            );
        }
    }

    #[test]
    fn with_reminders_off_a_stray_reminder_call_is_an_unknown_tool() {
        let call = reminder(json!({"title": "Chiamare Luca", "day": null, "time": null}));
        assert_eq!(
            validate(&call, false),
            Err("the reply asked for an unknown tool (create_reminder).".into())
        );
    }
}
