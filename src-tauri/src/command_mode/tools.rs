//! The tool allowlist (ADR-0002): what command mode's LLM may ask Parla to do
//! instead of replying with text. Each tool is a strict JSON schema, a
//! validator and an executor. Tools join one at a time and never include a
//! shell or anything destructive. v1 holds `open_url` only (PARLA-65).

use super::{responses::ToolCall, UrlOpener};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};

pub const OPEN_URL: &str = "open_url";

/// A tool call that passed the allowlist and its validator, ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// An http or https page, for the default browser.
    OpenUrl(Url),
}

/// The allowlist and the validators: an unknown tool, malformed arguments or
/// an address that is not an http(s) page is refused, with the reason.
pub fn validate(call: &ToolCall) -> Result<Action, String> {
    match call.name.as_str() {
        OPEN_URL => web_page(&call.arguments).map(Action::OpenUrl),
        name => Err(format!("the reply asked for an unknown tool ({name}).")),
    }
}

impl Action {
    /// The executor.
    pub fn run(&self, urls: &dyn UrlOpener) -> Result<(), String> {
        match self {
            Action::OpenUrl(url) => urls.open(url.as_str()),
        }
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
fn web_page(arguments: &str) -> Result<Url, String> {
    let arguments: OpenUrlArguments = serde_json::from_str(arguments)
        .map_err(|_| "the reply's open_url call could not be read.".to_string())?;
    let url = Url::parse(arguments.url.trim())
        .map_err(|_| "the reply's address is not a valid web address.".to_string())?;
    match url.scheme() {
        "http" | "https" if url.host_str().is_some_and(|host| !host.is_empty()) => Ok(url),
        "http" | "https" => Err("the reply's address is not a valid web address.".into()),
        scheme => Err(format!(
            "refused to open a {scheme}: address; only web pages open."
        )),
    }
}

/// The `tools` the request declares, strict so the arguments always match
/// the schema.
pub fn declarations() -> Value {
    json!([{
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
    }])
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
        validate(&call(OPEN_URL, url)).map(|Action::OpenUrl(url)| url.to_string())
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
        let Ok(Action::OpenUrl(url)) =
            validate(&call(OPEN_URL, "https://www.google.com/search?q=x"))
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
        assert!(validate(&call("run_shell", "https://example.com")).is_err());
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
            assert!(validate(&call).is_err(), "{arguments:?}");
        }
    }
}
