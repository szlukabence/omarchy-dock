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

    /// Resolve a pinned id to its desktop entry.
    ///
    /// Pins are documented as "desktop-entry ids or window classes", so an id
    /// that names no desktop file is tried as a declared `StartupWMClass` too.
    /// That is what keeps a pin working when an application's desktop file is
    /// renamed or replaced: removing Omarchy's Hermes TUI (`Hermes.desktop`)
    /// left a pin named `Hermes`, and the desktop app that remained ships as
    /// `hermes-desktop.desktop` with `StartupWMClass=Hermes`.
    ///
    /// Exact keys only. `match_class` also falls back to the last dotted
    /// segment, which suits reported window classes but would let a pin bind to
    /// an unrelated application that happens to share a word.
    pub fn resolve_pin(&self, id: &str) -> Option<&Entry> {
        self.by_id(id).or_else(|| {
            self.keys.get(&id.to_lowercase()).map(|i| &self.entries[*i])
        })
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

        if let Some(e) = self.resolve_pin(id) {
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
///
/// Chromium builds it from the URL's *host* and *path* only: no scheme, no
/// port, no login, no query, no fragment. The port matters in practice —
/// self-hosted web apps usually have one. `http://100.64.0.11:8080` really
/// produces `chrome-100.64.0.11__-Default`, verified against a live window;
/// keeping the port meant the window never matched its own desktop entry, so
/// the dock showed a bare IP with a generic icon.
pub fn chromium_app_class(exec: &str) -> Option<String> {
    let url = webapp_url(exec)?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    // Query and fragment are not part of the path Chromium uses.
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };
    let host = host_of(authority);
    if host.is_empty() {
        return None;
    }
    // Leading slash already removed by the split; inner slashes become `_`.
    let path = path.replace('/', "_");
    Some(format!("chrome-{host}__{path}-Default").to_lowercase())
}

/// The host part of a URL authority: without `user@`, and without `:port`.
///
/// An IPv6 literal keeps its brackets, as Chromium's host does, and only a
/// colon *after* the closing bracket starts a port.
fn host_of(authority: &str) -> &str {
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if host.starts_with('[') {
        return match host.find(']') {
            Some(end) => &host[..=end],
            None => host,
        };
    }
    host.split(':').next().unwrap_or(host)
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

/// The host of the site a web-app `Exec=` line opens, lowercased.
pub fn webapp_host(exec: &str) -> Option<String> {
    let url = webapp_url(exec)?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = host_of(authority).to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
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
    fn a_port_is_not_part_of_the_web_app_class() {
        // Termix: a self-hosted web app on an IP and port. The live window's
        // class was chrome-100.64.0.11__-Default.
        let exec = r#"omarchy-launch-webapp "http://100.64.0.11:8080""#;
        assert_eq!(chromium_app_class(exec).unwrap(), "chrome-100.64.0.11__-default");

        let m = Matcher::build(vec![entry("Termix", None, exec)]);
        let e = m.match_class("chrome-100.64.0.11__-Default").expect("window matches its entry");
        assert_eq!(e.id, "Termix");
    }

    #[test]
    fn login_query_and_fragment_are_ignored_too() {
        assert_eq!(
            chromium_app_class("chromium --app=https://me@example.com:8443/a/b?x=1#top").unwrap(),
            "chrome-example.com__a_b-default"
        );
    }

    #[test]
    fn an_ipv6_host_keeps_its_brackets() {
        assert_eq!(host_of("[::1]:8080"), "[::1]");
        assert_eq!(host_of("[::1]"), "[::1]");
        assert_eq!(host_of("user@host:1"), "host");
    }

    fn entry(id: &str, wm_class: Option<&str>, exec: &str) -> Entry {
        Entry {
            id: id.into(),
            name: id.into(),
            icon: id.into(),
            exec: exec.into(),
            wm_class: wm_class.map(Into::into),
            actions: Vec::new(),
            no_display: false,
            path: Default::default(),
            terminal: false,
        }
    }

    #[test]
    fn a_pin_falls_back_to_the_declared_window_class() {
        // The Hermes case: the pin was the TUI's desktop id; the desktop app
        // that remains has a different id but declares that class.
        let m = Matcher::build(vec![entry("hermes-desktop", Some("Hermes"), "hermes-desktop %U")]);
        let e = m.resolve_pin("Hermes").expect("resolves through StartupWMClass");
        assert_eq!(e.id, "hermes-desktop");
        assert!(m.expected_classes("Hermes").contains(&"hermes".to_string()));
    }

    #[test]
    fn a_desktop_id_still_wins_over_a_class() {
        let m = Matcher::build(vec![
            entry("Hermes", None, "hermes-tui"),
            entry("hermes-desktop", Some("Hermes"), "hermes-desktop"),
        ]);
        // Exact desktop id first — the key map would otherwise answer.
        assert_eq!(m.by_id("Hermes").unwrap().exec, "hermes-tui");
        assert_eq!(m.resolve_pin("Hermes").unwrap().exec, "hermes-tui");
    }

    #[test]
    fn a_pin_never_binds_through_a_partial_name() {
        // match_class would accept this via the last dotted segment; a pin
        // must not silently turn into a different application.
        let m = Matcher::build(vec![entry("app", None, "app")]);
        assert!(m.match_class("org.example.app").is_some());
        assert!(m.resolve_pin("org.example.app").is_none());
    }

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
