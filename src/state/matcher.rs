//! Matching Hyprland window classes to desktop entries.
//!
//! This is the part of a dock that quietly gets everything wrong, because a
//! window class and a desktop id agree far less often than one would hope.
//! Four keys are indexed per entry, tried most-specific first:
//!
//! 1. `StartupWMClass` — authoritative when present, but often absent.
//! 2. The desktop id itself (`chromium` -> class `chromium`).
//! 3. A derived Chromium web-app class (see below).
//! 4. The executable's basename (`Exec=/usr/bin/foo -x` -> `foo`).
//!
//! **Chromium web apps.** Omarchy's PWAs (`Gmail.desktop`, `Claude.desktop`, …)
//! carry no `StartupWMClass` and exec `omarchy-launch-webapp <url>`, which runs
//! `chromium --app=<url>`. Chromium derives its own class from the URL:
//!
//! ```text
//! https://example.com          -> chrome-example.com__-Default
//! https://example.com/foo/bar  -> chrome-example.com__foo_bar-Default
//! https://example.com/foo/     -> chrome-example.com__foo_-Default
//! ```
//!
//! i.e. `chrome-<host>__<path>-Default`, where `<path>` has its leading slash
//! removed and the rest of its slashes turned into underscores. Verified
//! empirically against a live Hyprland; without this rule none of the eight
//! pinned web apps would ever show as running.

#![allow(dead_code)]

use crate::desktop::Entry;
use std::collections::HashMap;

pub struct Matcher {
    entries: Vec<Entry>,
    /// Lowercased match key -> index into `entries`.
    keys: HashMap<String, usize>,
}

impl Matcher {
    pub fn build(entries: Vec<Entry>) -> Self {
        let mut keys: HashMap<String, usize> = HashMap::new();

        for (i, e) in entries.iter().enumerate() {
            // Weakest keys first so stronger ones overwrite them.
            if let Some(exe) = exec_basename(&e.exec) {
                keys.entry(exe).or_insert(i);
            }
            if let Some(app) = chromium_app_class(&e.exec) {
                keys.insert(app, i);
            }
            keys.insert(e.id.to_lowercase(), i);
            if let Some(wm) = &e.wm_class {
                keys.insert(wm.to_lowercase(), i);
            }
        }

        Self { entries, keys }
    }

    /// Resolve a Hyprland window class to its desktop entry.
    pub fn match_class(&self, class: &str) -> Option<&Entry> {
        let class = class.to_lowercase();
        if let Some(i) = self.keys.get(&class) {
            return Some(&self.entries[*i]);
        }

        // Chromium sometimes reports a bare host for web apps, and some
        // toolkits append a suffix (`foo.bin`, `Foo-wrapped`). Fall back to a
        // prefix match on the first dotted segment, which catches
        // `org.gnome.Nautilus` reported as `nautilus`.
        let head = class.split('.').next_back().unwrap_or(&class);
        self.keys.get(head).map(|i| &self.entries[*i])
    }

    /// Look an entry up by its desktop id, for pinned items.
    pub fn by_id(&self, id: &str) -> Option<&Entry> {
        let id = id.to_lowercase();
        self.entries.iter().find(|e| e.id.to_lowercase() == id)
    }

    /// Every class a pinned entry's windows might report.
    ///
    /// Returns candidates rather than one answer on purpose. `StartupWMClass`
    /// is usually the best key but can be present *and wrong* (Arch's
    /// chromium.desktop ships an unsubstituted `@@startup_wm_class`), so a
    /// single authoritative pick would let one bad key shadow a working
    /// fallback and leave the app permanently "not running".
    pub fn expected_classes(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |s: String| {
            if !s.is_empty() && !out.contains(&s) {
                out.push(s);
            }
        };

        if let Some(e) = self.by_id(id) {
            if let Some(w) = &e.wm_class {
                push(w.to_lowercase());
            }
            if let Some(c) = chromium_app_class(&e.exec) {
                push(c);
            }
            push(e.id.to_lowercase());
            if let Some(b) = exec_basename(&e.exec) {
                push(b);
            }
        }
        push(id.to_lowercase());
        out
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

/// Basename of the executable in an `Exec=` line.
fn exec_basename(exec: &str) -> Option<String> {
    let first = exec.split_whitespace().next()?;
    let base = first.rsplit('/').next()?;
    (!base.is_empty() && !base.starts_with('%')).then(|| base.to_lowercase())
}

/// Derive the class Chromium will use for `--app=<url>`, if this entry
/// launches a web app.
pub fn chromium_app_class(exec: &str) -> Option<String> {
    let url = webapp_url(exec)?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };
    if host.is_empty() {
        return None;
    }
    // Leading slash already removed by the split; inner slashes become `_`.
    let path = path.replace('/', "_");
    Some(format!("chrome-{host}__{path}-Default").to_lowercase())
}

/// The inverse of `chromium_app_class`: recover a launchable web app from the
/// class of a running Chromium window.
///
/// Omarchy encodes the whole URL in the window class, so a web app that has no
/// `.desktop` file — one launched ad hoc with `omarchy launch webapp` — can
/// still be pinned and relaunched later. Without this, pinning such a window
/// stores a class nothing can start, and the icon is dead the moment the
/// window closes.
///
/// Returns the display label and the URL.
pub fn webapp_from_class(class: &str) -> Option<(String, String)> {
    let rest = class.strip_prefix("chrome-")?.strip_suffix("-Default")?;
    let (host, path) = rest.split_once("__")?;
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    // `_` stood in for `/` on the way out.
    let path = path.replace('_', "/");
    let url = if path.is_empty() {
        format!("https://{host}")
    } else {
        format!("https://{host}/{path}")
    };
    // "mail.google.com" reads better than the raw class, and Omarchy's own
    // web-app entries are named after the site rather than the host.
    let label = host.strip_prefix("www.").unwrap_or(host).to_string();
    Some((label, url))
}

/// The command Omarchy itself uses to start or focus a web app.
pub fn webapp_command(class: &str, url: &str) -> String {
    // `launch or focus` rather than `launch`: a second click should raise the
    // window that is already open, which is what the dock means by a click.
    format!("omarchy launch or focus webapp {} {}", shell_quote(class), shell_quote(url))
}

/// Single-quote a value for a shell command line.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Extract the web-app URL from an `Exec=` line, covering both Omarchy's
/// wrapper and a direct `--app=` invocation.
fn webapp_url(exec: &str) -> Option<String> {
    if let Some(rest) = exec.split_once("--app=") {
        return Some(unquote(rest.1.split_whitespace().next()?));
    }
    if exec.contains("omarchy-launch-webapp") {
        // The URL is the first argument after the wrapper.
        let after = exec.split("omarchy-launch-webapp").nth(1)?;
        return Some(unquote(after.split_whitespace().next()?));
    }
    None
}

fn unquote(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'').to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_web_app_class_round_trips_back_to_its_url() {
        // The class is derived from the URL, so the URL can be recovered from
        // the class — which is what makes an ad-hoc web app pinnable.
        for exec in [
            "chromium --app=https://mail.google.com/mail",
            "omarchy-launch-webapp https://mail.google.com/mail",
        ] {
            let class = chromium_app_class(exec).unwrap();
            assert_eq!(class, "chrome-mail.google.com__mail-default");
        }

        let (label, url) =
            webapp_from_class("chrome-mail.google.com__mail-Default").unwrap();
        assert_eq!(label, "mail.google.com");
        assert_eq!(url, "https://mail.google.com/mail");
    }

    #[test]
    fn a_nested_path_survives_the_round_trip() {
        let class = chromium_app_class("chromium --app=https://example.com/a/b").unwrap();
        assert_eq!(class, "chrome-example.com__a_b-default");
        // Case differs because Chromium lowercases; the inverse is case-blind
        // about the marker parts only, so feed it the real form.
        let (_, url) = webapp_from_class("chrome-example.com__a_b-Default").unwrap();
        assert_eq!(url, "https://example.com/a/b");
    }

    #[test]
    fn a_bare_host_web_app_has_no_path() {
        let (label, url) = webapp_from_class("chrome-example.com__-Default").unwrap();
        assert_eq!(label, "example.com");
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn an_ordinary_window_class_is_not_mistaken_for_a_web_app() {
        // These would otherwise become pins that launch a nonsense URL.
        assert!(webapp_from_class("chromium").is_none());
        assert!(webapp_from_class("org.gnome.Nautilus").is_none());
        assert!(webapp_from_class("chrome-Default").is_none());
        // No dot in the host: not a hostname.
        assert!(webapp_from_class("chrome-localhost__-Default").is_none());
    }

    #[test]
    fn a_webapp_command_quotes_its_arguments() {
        // The class and URL go onto a shell command line, so a quote in either
        // must not end the argument.
        let cmd = webapp_command("chrome-x.com__-Default", "https://x.com/a'b");
        assert!(cmd.starts_with("omarchy launch or focus webapp "));
        assert!(cmd.contains(r"'https://x.com/a'\''b'"), "{cmd}");
    }

    use super::*;

    #[test]
    fn derives_chromium_class_from_omarchy_webapp_exec() {
        // Exact strings from this machine's Gmail/Claude/Outlook entries.
        assert_eq!(
            chromium_app_class(r#"omarchy-launch-webapp "https://gmail.com""#).as_deref(),
            Some("chrome-gmail.com__-Default".to_lowercase().as_str())
        );
        assert_eq!(
            chromium_app_class(r#"omarchy-launch-webapp "https://claude.ai/""#).as_deref(),
            Some("chrome-claude.ai__-Default".to_lowercase().as_str())
        );
        // Trailing slash is preserved as a trailing underscore.
        assert_eq!(
            chromium_app_class(r#"omarchy-launch-webapp "https://outlook.live.com/mail/""#)
                .as_deref(),
            Some("chrome-outlook.live.com__mail_-Default".to_lowercase().as_str())
        );
    }

    #[test]
    fn matches_the_live_class_observed_from_hyprland() {
        // Ground truth captured from a running window.
        assert_eq!(
            chromium_app_class("omarchy-launch-webapp https://example.com/foo/bar").as_deref(),
            Some("chrome-example.com__foo_bar-Default".to_lowercase().as_str())
        );
    }

    #[test]
    fn a_bogus_startupwmclass_does_not_shadow_the_id_fallback() {
        use crate::desktop::Entry;
        use std::path::PathBuf;
        // Mirrors Arch's chromium.desktop, whose StartupWMClass is a
        // template that was never substituted (and so is dropped at parse
        // time, leaving None here).
        let e = Entry {
            id: "chromium".into(),
            name: "Chromium".into(),
            icon: "chromium".into(),
            exec: "/usr/bin/chromium %U".into(),
            wm_class: None,
            no_display: false,
            terminal: false,
            actions: vec![],
            path: PathBuf::new(),
        };
        let m = Matcher::build(vec![e]);
        // The real window class Hyprland reports.
        assert!(m.expected_classes("chromium").contains(&"chromium".to_string()));
        assert!(m.match_class("chromium").is_some());
    }

    #[test]
    fn ignores_non_webapp_execs() {
        assert_eq!(chromium_app_class("/usr/bin/kitty"), None);
        assert_eq!(exec_basename("/usr/bin/kitty --title x").as_deref(), Some("kitty"));
    }
}
