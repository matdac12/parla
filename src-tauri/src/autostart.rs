//! "Start Parla with Windows" (PARLA-52): one string value in the current
//! user's `Run` key, pointing at this exe. Parla starts into the tray without a
//! window, so the value needs no arguments. The registry is the only state;
//! nothing is kept in settings.json.
//!
//! A `Parla` shortcut in the Startup folder (what Parla used before this
//! setting) starts it too. Turning the setting on next to one would start Parla
//! twice at login, so it is refused until the shortcut is gone. Parla never
//! deletes it.
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The value's name under `Run`; shown by Task Manager's Startup tab.
const VALUE_NAME: &str = "Parla";

/// The registry operations behind the setting, so the logic is tested without
/// touching the real `Run` key.
pub trait RunKey {
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    /// Removing a value that is not there succeeds.
    fn remove(&self, name: &str) -> Result<(), String>;
}

/// What the Settings toggle and the wizard step show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// False off Windows, where there is nothing to register.
    pub supported: bool,
    /// The `Run` value exists.
    pub enabled: bool,
    /// The value runs something other than this exe (Parla moved, or another
    /// copy set it).
    pub stale: bool,
    /// What the value runs, when it exists.
    pub command: Option<String>,
    /// The file name of a Parla shortcut in the Startup folder, if any.
    pub shortcut: Option<String>,
}

/// The `Run` value for `exe`: quoted, so a path with spaces runs.
fn command_for(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn runs(command: &str, exe: &Path) -> bool {
    command
        .trim()
        .trim_matches('"')
        .eq_ignore_ascii_case(&exe.display().to_string())
}

/// A Parla shortcut in the Startup folder `dir`: `Parla.lnk`, or what Explorer
/// names a copy of it (`Parla - Shortcut.lnk`). The target is not read.
pub fn startup_shortcut(dir: &Path) -> Option<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            lower.starts_with("parla") && (lower.ends_with(".lnk") || lower.ends_with(".url"))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

pub fn status(key: &dyn RunKey, exe: &Path, startup_dir: Option<&Path>) -> Result<Status, String> {
    let command = key
        .get(VALUE_NAME)?
        .filter(|value| !value.trim().is_empty());
    Ok(Status {
        supported: cfg!(windows),
        enabled: command.is_some(),
        stale: command.as_deref().is_some_and(|value| !runs(value, exe)),
        command,
        shortcut: startup_dir.and_then(startup_shortcut),
    })
}

/// Turns the setting on (pointing at `exe`, also repairing a stale value) or
/// off. On is refused while a Startup shortcut exists.
pub fn set(
    key: &dyn RunKey,
    exe: &Path,
    startup_dir: Option<&Path>,
    enabled: bool,
) -> Result<Status, String> {
    if enabled {
        if let Some(shortcut) = startup_dir.and_then(startup_shortcut) {
            return Err(format!(
                "{shortcut} in your Startup folder already starts Parla at login. Delete it first, \
                 or Parla would start twice."
            ));
        }
        key.set(VALUE_NAME, &command_for(exe))?;
    } else {
        key.remove(VALUE_NAME)?;
    }
    status(key, exe, startup_dir)
}

/// The per-user Startup folder.
pub fn startup_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(Path::new(&appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
}

/// The real `Run` key.
pub fn run_key() -> Registry {
    Registry::new(r"Software\Microsoft\Windows\CurrentVersion\Run")
}

pub fn current_status() -> Result<Status, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    status(&run_key(), &exe, startup_dir().as_deref())
}

pub fn set_current(enabled: bool) -> Result<Status, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    set(&run_key(), &exe, startup_dir().as_deref(), enabled)
}

#[cfg(windows)]
pub use registry::Registry;

#[cfg(windows)]
mod registry {
    //! The few advapi32 calls needed, declared here because the crate's
    //! windows-sys features do not include the registry and Cargo.toml stays as
    //! it is. `raw-dylib` links them the way windows-sys does.
    use super::RunKey;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    type Hkey = isize;
    /// `(HKEY)(LONG)0x80000001`: sign-extended, as in winreg.h.
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001_u32 as i32 as isize;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    #[link(name = "advapi32", kind = "raw-dylib")]
    extern "system" {
        fn RegCreateKeyExW(
            key: Hkey,
            subkey: *const u16,
            reserved: u32,
            class: *const u16,
            options: u32,
            access: u32,
            security: *const c_void,
            result: *mut Hkey,
            disposition: *mut u32,
        ) -> i32;
        fn RegOpenKeyExW(
            key: Hkey,
            subkey: *const u16,
            options: u32,
            access: u32,
            result: *mut Hkey,
        ) -> i32;
        fn RegSetValueExW(
            key: Hkey,
            name: *const u16,
            reserved: u32,
            kind: u32,
            data: *const u8,
            len: u32,
        ) -> i32;
        fn RegQueryValueExW(
            key: Hkey,
            name: *const u16,
            reserved: *const u32,
            kind: *mut u32,
            data: *mut u8,
            len: *mut u32,
        ) -> i32;
        fn RegDeleteValueW(key: Hkey, name: *const u16) -> i32;
        #[cfg(test)]
        fn RegDeleteKeyW(key: Hkey, subkey: *const u16) -> i32;
        fn RegCloseKey(key: Hkey) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    fn check(code: i32, what: &str) -> Result<(), String> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("{what} failed (Windows error {code})"))
        }
    }

    /// An open key, closed on drop.
    struct Open(Hkey);
    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: a handle this process opened and has not closed.
            unsafe { RegCloseKey(self.0) };
        }
    }

    /// A key under `HKCU`, named by its path from there.
    pub struct Registry {
        subkey: String,
    }

    impl Registry {
        pub fn new(subkey: &str) -> Self {
            Self {
                subkey: subkey.into(),
            }
        }

        /// `None` when the key does not exist.
        fn open(&self, access: u32) -> Result<Option<Open>, String> {
            let mut handle: Hkey = 0;
            // SAFETY: a NUL-terminated subkey and a valid out pointer.
            let code = unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    wide(&self.subkey).as_ptr(),
                    0,
                    access,
                    &mut handle,
                )
            };
            if code == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            check(code, "Opening the registry key")?;
            Ok(Some(Open(handle)))
        }

        /// Removes the (empty) key itself; the tests clean up with it.
        #[cfg(test)]
        pub fn delete_key(&self) -> Result<(), String> {
            // SAFETY: a NUL-terminated subkey.
            let code = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(&self.subkey).as_ptr()) };
            if code == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            check(code, "Deleting the registry key")
        }
    }

    impl RunKey for Registry {
        fn get(&self, name: &str) -> Result<Option<String>, String> {
            let Some(key) = self.open(KEY_QUERY_VALUE)? else {
                return Ok(None);
            };
            let name = wide(name);
            let mut kind = 0_u32;
            let mut len = 0_u32;
            // SAFETY: a size query: no data buffer, valid out pointers.
            let code = unsafe {
                RegQueryValueExW(
                    key.0,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    std::ptr::null_mut(),
                    &mut len,
                )
            };
            if code == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            check(code, "Reading the registry value")?;
            if kind != REG_SZ && kind != REG_EXPAND_SZ {
                // Something starts at login under this name, just not text
                // Parla wrote: report it, never as "off".
                return Ok(Some("(a value Parla cannot read)".into()));
            }
            let mut data = vec![0_u16; (len as usize).div_ceil(2)];
            let mut size = (data.len() * 2) as u32;
            // SAFETY: `data` holds `size` bytes.
            let code = unsafe {
                RegQueryValueExW(
                    key.0,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    data.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            check(code, "Reading the registry value")?;
            data.truncate(size as usize / 2);
            while data.last() == Some(&0) {
                data.pop();
            }
            Ok(Some(String::from_utf16_lossy(&data)))
        }

        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            let mut handle: Hkey = 0;
            // SAFETY: a NUL-terminated subkey and valid out pointers; the key
            // is created when it is missing.
            let code = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(&self.subkey).as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut handle,
                    std::ptr::null_mut(),
                )
            };
            check(code, "Opening the registry key")?;
            let key = Open(handle);
            let data = wide(value);
            // SAFETY: `data` is `len` bytes of NUL-terminated UTF-16.
            let code = unsafe {
                RegSetValueExW(
                    key.0,
                    wide(name).as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            };
            check(code, "Writing the registry value")
        }

        fn remove(&self, name: &str) -> Result<(), String> {
            let Some(key) = self.open(KEY_SET_VALUE)? else {
                return Ok(());
            };
            // SAFETY: a NUL-terminated value name.
            let code = unsafe { RegDeleteValueW(key.0, wide(name).as_ptr()) };
            if code == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            check(code, "Removing the registry value")
        }
    }
}

/// Off Windows there is no `Run` key: nothing is ever enabled.
#[cfg(not(windows))]
pub struct Registry;

#[cfg(not(windows))]
impl Registry {
    fn new(_subkey: &str) -> Self {
        Self
    }
}

#[cfg(not(windows))]
impl RunKey for Registry {
    fn get(&self, _name: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn set(&self, _name: &str, _value: &str) -> Result<(), String> {
        Err("Start with Windows is only available on Windows".into())
    }
    fn remove(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    #[derive(Default)]
    struct Fake(RefCell<HashMap<String, String>>);
    impl RunKey for Fake {
        fn get(&self, name: &str) -> Result<Option<String>, String> {
            Ok(self.0.borrow().get(name).cloned())
        }
        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            self.0.borrow_mut().insert(name.into(), value.into());
            Ok(())
        }
        fn remove(&self, name: &str) -> Result<(), String> {
            self.0.borrow_mut().remove(name);
            Ok(())
        }
    }

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Program Files\Parla\parla.exe")
    }

    #[test]
    fn enabling_writes_the_quoted_exe_path_and_disabling_removes_it() {
        let key = Fake::default();
        let off = status(&key, &exe(), None).unwrap();
        assert!(!off.enabled && !off.stale && off.command.is_none());

        let on = set(&key, &exe(), None, true).unwrap();
        assert!(on.enabled && !on.stale);
        assert_eq!(
            key.get(VALUE_NAME).unwrap().as_deref(),
            Some(r#""C:\Program Files\Parla\parla.exe""#)
        );

        let off = set(&key, &exe(), None, false).unwrap();
        assert!(!off.enabled);
        assert_eq!(key.get(VALUE_NAME).unwrap(), None);
        // Turning off twice is fine.
        set(&key, &exe(), None, false).unwrap();
    }

    #[test]
    fn a_value_for_another_copy_is_stale_and_enabling_repairs_it() {
        let key = Fake::default();
        key.set(VALUE_NAME, r#""D:\old\parla.exe""#).unwrap();
        let before = status(&key, &exe(), None).unwrap();
        assert!(before.enabled && before.stale);
        assert_eq!(before.command.as_deref(), Some(r#""D:\old\parla.exe""#));

        let after = set(&key, &exe(), None, true).unwrap();
        assert!(after.enabled && !after.stale);

        // The same path in another case, or unquoted, is not stale.
        key.set(VALUE_NAME, r"c:\program files\parla\PARLA.EXE")
            .unwrap();
        assert!(!status(&key, &exe(), None).unwrap().stale);
        // Extra arguments are not what Parla writes.
        key.set(VALUE_NAME, r#""C:\Program Files\Parla\parla.exe" --x"#)
            .unwrap();
        assert!(status(&key, &exe(), None).unwrap().stale);
    }

    #[test]
    fn a_blank_value_counts_as_off() {
        let key = Fake::default();
        key.set(VALUE_NAME, "  ").unwrap();
        assert!(!status(&key, &exe(), None).unwrap().enabled);
    }

    #[test]
    fn a_startup_shortcut_blocks_turning_on_and_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let shortcut = dir.path().join("Parla.lnk");
        std::fs::write(&shortcut, b"lnk").unwrap();
        std::fs::write(dir.path().join("Other.lnk"), b"lnk").unwrap();
        let key = Fake::default();

        let before = status(&key, &exe(), Some(dir.path())).unwrap();
        assert_eq!(before.shortcut.as_deref(), Some("Parla.lnk"));
        assert!(!before.enabled);

        let error = set(&key, &exe(), Some(dir.path()), true).unwrap_err();
        assert!(
            error.contains("Parla.lnk") && error.contains("twice"),
            "{error}"
        );
        assert_eq!(key.get(VALUE_NAME).unwrap(), None, "nothing was written");
        assert!(shortcut.exists(), "the shortcut is the user's to delete");

        // Turning off never needs the shortcut gone.
        set(&key, &exe(), Some(dir.path()), false).unwrap();
        assert!(shortcut.exists());
    }

    #[test]
    fn both_a_value_and_a_shortcut_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Parla.lnk"), b"lnk").unwrap();
        let key = Fake::default();
        key.set(VALUE_NAME, &command_for(&exe())).unwrap();
        let both = status(&key, &exe(), Some(dir.path())).unwrap();
        assert!(both.enabled && both.shortcut.is_some());
        // Turning it off still works with the shortcut there.
        assert!(!set(&key, &exe(), Some(dir.path()), false).unwrap().enabled);
    }

    #[test]
    fn shortcut_detection_matches_parla_links_only() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(startup_shortcut(dir.path()), None);
        assert_eq!(startup_shortcut(&dir.path().join("missing")), None);
        for name in ["Tasca.lnk", "parla.txt", "Annota.lnk"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        assert_eq!(startup_shortcut(dir.path()), None);
        std::fs::write(dir.path().join("parla - Shortcut.LNK"), b"x").unwrap();
        assert_eq!(
            startup_shortcut(dir.path()).as_deref(),
            Some("parla - Shortcut.LNK")
        );
    }

    /// The real registry calls, on a throwaway key under HKCU (never the
    /// `Run` key), removed afterwards even if an assertion fails.
    #[cfg(windows)]
    #[test]
    fn the_registry_round_trips_a_value_on_a_throwaway_key() {
        struct Cleanup(Registry);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.remove(VALUE_NAME);
                let _ = self.0.delete_key();
            }
        }
        let key = Cleanup(Registry::new(&format!(
            r"Software\com.matdac6.parla.test-{}",
            std::process::id()
        )));
        let value = r#""C:\Users\Zoë Müller\parla.exe""#;
        assert_eq!(key.0.get(VALUE_NAME).unwrap(), None, "no key yet");
        key.0.remove(VALUE_NAME).unwrap();
        key.0.set(VALUE_NAME, value).unwrap();
        assert_eq!(key.0.get(VALUE_NAME).unwrap().as_deref(), Some(value));
        key.0.set(VALUE_NAME, "short").unwrap();
        assert_eq!(key.0.get(VALUE_NAME).unwrap().as_deref(), Some("short"));
        key.0.remove(VALUE_NAME).unwrap();
        assert_eq!(key.0.get(VALUE_NAME).unwrap(), None);
        key.0.remove(VALUE_NAME).unwrap();
    }
}
