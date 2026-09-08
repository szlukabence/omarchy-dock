//! Application bootstrap: owns config, palette, the CSS provider, and the set
//! of dock surfaces, and drains the event channel on the GTK main thread.

use gtk4 as gtk;

use gtk::prelude::*;
use gtk::{gdk, glib};

use std::cell::RefCell;
use std::rc::Rc;

use crate::config::Config;
use crate::event::{self, AppEvent};
use crate::hypr::events::HyprEvent;
use crate::state::DockState;
use crate::theme::{css, Palette};
use crate::state::DockItem;
use crate::ui::DockSurface;

pub const APP_ID: &str = "dev.omarchy.Dock";

struct App {
    cfg: Config,
    /// Reconciles pins against live Hyprland windows.
    state: DockState,
    /// Nudges the async worker to re-query `j/clients`.
    snapshot_tx: Option<crate::runtime::SnapshotRequest>,
    palette: Palette,
    /// One provider, reloaded in place. Adding a new provider per reload would
    /// stack styles and leak the old ones.
    provider: gtk::CssProvider,
    docks: Vec<DockSurface>,
}

impl App {
    fn restyle(&mut self) {
        self.palette = if self.cfg.theme.follow_omarchy {
            Palette::current()
        } else {
            Palette::default()
        };
        self.apply_icon_theme();
        let sheet = css::generate(&self.cfg, &self.palette);
        self.provider.load_from_string(&sheet);
        tracing::info!(theme = %self.palette.name, "styles reloaded");
    }

    /// Point GTK at the icon theme the user configured, or the one the active
    /// Omarchy theme requests. Extra search paths are prepended so a user's
    /// own icons win over system ones.
    fn apply_icon_theme(&self) {
        let Some(display) = gdk::Display::default() else { return };
        let icons = gtk::IconTheme::for_display(&display);

        for path in &self.cfg.theme.icon_paths {
            icons.add_search_path(crate::config::expand_tilde(path));
        }

        let name = if !self.cfg.theme.icon_theme.is_empty() {
            Some(self.cfg.theme.icon_theme.clone())
        } else if self.cfg.theme.follow_omarchy {
            crate::theme::icon_theme()
        } else {
            None
        };

        if let Some(name) = name {
            if icons.theme_name() != name.as_str() {
                tracing::info!(theme = %name, "icon theme set");
                icons.set_theme_name(Some(&name));
            }
        }
    }

    /// Fold one Hyprland event into dock state, rebuilding only when the
    /// rendered item set could actually have changed.
    ///
    /// Deltas are applied locally rather than re-querying, so a busy desktop
    /// costs no IPC round-trips. `OpenWindow` is the exception: the event
    /// carries less than `j/clients` (no workspace id, geometry or pid), so a
    /// snapshot is requested instead of inventing a partial `Client`.
    fn on_hypr(&mut self, event: HyprEvent, gtk_app: &gtk::Application) {
        use HyprEvent::*;
        let dirty = match event {
            OpenWindow { .. } | Reconnected => {
                // Ask the worker for a fresh snapshot; it arrives as
                // HyprSnapshot and rebuilds then.
                self.request_snapshot();
                false
            }
            CloseWindow(addr) => {
                self.state.remove_client(&addr);
                true
            }
            ActiveWindowAddr(addr) => {
                self.state.set_focused(addr);
                true
            }
            Urgent(addr) => {
                self.state.set_urgent(addr);
                true
            }
            WindowTitle { addr, title } => {
                // Titles only show in tooltips, so no relayout is needed.
                self.state.set_title(&addr, title);
                false
            }
            MoveWindow { .. } | ActiveSpecial { .. } => {
                self.request_snapshot();
                false
            }
            _ => false,
        };

        if dirty {
            self.rebuild(gtk_app);
        }
    }

    /// Ask the async worker to re-query the full window list.
    fn request_snapshot(&self) {
        if let Some(tx) = &self.snapshot_tx {
            let _ = tx.try_send(());
        }
    }

    fn rebuild(&mut self, gtk_app: &gtk::Application) {
        let mut items = self.state.items(&self.cfg.items.pinned, self.cfg.items.show_running);
        if self.cfg.items.show_trash {
            items.push(crate::state::DockItem {
                key: "__trash".into(),
                label: "Trash".into(),
                icon: "user-trash".into(),
                windows: Vec::new(),
                pinned: true,
                active: false,
                urgent: false,
                scratchpad: false,
            });
        }

        for d in self.docks.drain(..) {
            d.close();
        }
        self.docks = build_docks(gtk_app, &self.cfg, &items);
        if tracing::enabled!(tracing::Level::DEBUG) {
            for i in &items {
                tracing::debug!(
                    key = %i.key, icon = %i.icon, pinned = i.pinned,
                    windows = i.windows.len(), active = i.active, "item"
                );
            }
        }
        tracing::debug!(surfaces = self.docks.len(), items = items.len(), "dock rebuilt");
    }
}

pub fn run() -> glib::ExitCode {
    let gtk_app = gtk::Application::builder().application_id(APP_ID).build();
    let (tx, rx) = event::channel();

    // Each worker owns its own thread and never touches GTK.
    if let Err(e) = crate::config::watcher::spawn(tx.clone()) {
        tracing::error!(error = %e, "live reload unavailable");
    }
    let snapshot_tx = match crate::runtime::spawn(tx) {
        Ok(handle) => Some(handle),
        Err(e) => {
            tracing::error!(error = %e, "Hyprland IPC unavailable");
            None
        }
    };

    let state: Rc<RefCell<Option<App>>> = Rc::new(RefCell::new(None));

    gtk_app.connect_activate(move |gtk_app| {
        // `activate` can fire more than once; only build the first time.
        if state.borrow().is_some() {
            return;
        }

        let cfg = Config::load();
        let provider = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let entries = crate::desktop::scan();
        tracing::info!(count = entries.len(), "desktop entries scanned");

        let mut app = App {
            cfg,
            state: DockState::new(entries),
            snapshot_tx: snapshot_tx.clone(),
            palette: Palette::default(),
            provider,
            docks: Vec::new(),
        };
        // Style before building, so surfaces map already themed and the user
        // never sees an unstyled frame.
        app.restyle();
        app.rebuild(gtk_app);
        *state.borrow_mut() = Some(app);

        // Drain reload events on the main thread.
        let state = state.clone();
        let gtk_app = gtk_app.clone();
        let rx = rx.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let mut guard = state.borrow_mut();
                let Some(app) = guard.as_mut() else { continue };

                match event {
                    AppEvent::StyleChanged => app.restyle(),
                    AppEvent::HyprSnapshot(clients) => {
                        tracing::info!(windows = clients.len(), "window snapshot");
                        app.state.set_clients(clients);
                        app.rebuild(&gtk_app);
                    }
                    AppEvent::Hypr(e) => app.on_hypr(e, &gtk_app),
                    AppEvent::ConfigChanged => {
                        let next = Config::load();
                        // Geometry-affecting changes need new surfaces;
                        // anything else is just a restyle, which is far
                        // cheaper and preserves hover state.
                        let structural = needs_rebuild(&app.cfg, &next);
                        app.cfg = next;
                        app.restyle();
                        if structural {
                            app.rebuild(&gtk_app);
                        }
                    }
                }
            }
        });
    });

    gtk_app.run()
}

/// Whether a config change alters surface geometry or item set.
fn needs_rebuild(old: &Config, new: &Config) -> bool {
    old.dock.position != new.dock.position
        || old.dock.icon_size != new.dock.icon_size
        || old.dock.padding_x != new.dock.padding_x
        || old.dock.padding_y != new.dock.padding_y
        || old.dock.spacing != new.dock.spacing
        || old.dock.edge_offset != new.dock.edge_offset
        || old.dock.reserve_space != new.dock.reserve_space
        || old.magnify.enabled != new.magnify.enabled
        || old.magnify.zoom != new.magnify.zoom
        || old.magnify.lift != new.magnify.lift
        || old.items.pinned != new.items.pinned
        || old.items.show_trash != new.items.show_trash
        || old.monitors.mode != new.monitors.mode
        || old.monitors.primary != new.monitors.primary
}

/// Create one surface per monitor the config asks for.
fn build_docks(
    gtk_app: &gtk::Application,
    cfg: &Config,
    items: &[DockItem],
) -> Vec<DockSurface> {
    use crate::config::MonitorMode;

    let Some(display) = gdk::Display::default() else {
        return vec![DockSurface::build(gtk_app, cfg, items, None)];
    };
    let monitors = display.monitors();
    let all: Vec<gdk::Monitor> = (0..monitors.n_items())
        .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
        .collect();

    if all.is_empty() {
        return vec![DockSurface::build(gtk_app, cfg, items, None)];
    }

    match cfg.monitors.mode {
        MonitorMode::All => {
            all.iter().map(|m| DockSurface::build(gtk_app, cfg, items, Some(m))).collect()
        }
        // "Focused" follows the active output; until Hyprland IPC lands in
        // Phase 2 it behaves like "primary".
        MonitorMode::Primary | MonitorMode::Focused => {
            let chosen = all
                .iter()
                .find(|m| {
                    !cfg.monitors.primary.is_empty()
                        && m.connector().is_some_and(|c| c == cfg.monitors.primary)
                })
                .unwrap_or(&all[0]);
            vec![DockSurface::build(gtk_app, cfg, items, Some(chosen))]
        }
    }
}
