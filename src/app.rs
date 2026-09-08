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
use crate::theme::shell::Shell;
use crate::theme::{css, Palette};
use crate::state::DockItem;
use crate::ui::menu::MenuAction;
use crate::ui::DockSurface;

pub const APP_ID: &str = "dev.omarchy.Dock";

struct App {
    /// Config exactly as it sits on disk. Compared against a reload to decide
    /// whether a change is structural, and the basis `cfg` is derived from.
    raw_cfg: Config,
    /// `raw_cfg` with the active theme's shell scale folded into its pixel
    /// sizes. Everything that draws uses this one.
    cfg: Config,
    /// Reconciles pins against live Hyprland windows.
    state: DockState,
    /// Channels to the async worker: resync requests and user commands.
    worker: Option<crate::runtime::Handles>,
    palette: Palette,
    /// Design tokens from the active theme's `shell.toml`: the shapes, alphas
    /// and scale every other Omarchy surface is drawn with.
    shell: Shell,
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
        self.shell = if self.cfg.theme.follow_omarchy {
            Shell::current(&self.palette)
        } else {
            Shell::fallback(&self.palette)
        };
        self.cfg = effective(self.raw_cfg.clone(), &self.shell);
        self.apply_icon_theme();
        let sheet = css::generate(&self.cfg, &self.palette, &self.shell);
        self.provider.load_from_string(&sheet);
        tracing::info!(
            theme = %self.palette.name,
            style = ?self.cfg.theme.style,
            scale = self.shell.metrics.spacing_factor(),
            "styles reloaded"
        );
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
                // Focus drives both the active indicator and intelligent
                // hiding, and the new window's geometry may differ.
                self.request_snapshot();
                true
            }
            Urgent(addr) => {
                self.state.set_urgent(addr);
                true
            }
            // The snapshot carries each client's fullscreen state, and the
            // event only says that *something* changed.
            Fullscreen(_) => {
                self.request_snapshot();
                false
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
            // The strip shows which workspace is current and how full each one
            // is, so any of these changes it.
            Workspace { .. } | CreateWorkspace { .. } | DestroyWorkspace { .. } => {
                self.request_snapshot();
                false
            }
            _ => false,
        };

        if dirty {
            self.sync(gtk_app);
        }
    }

    /// Handle a command from `omarchy-dockctl`.
    fn on_control(&mut self, cmd: crate::ipc_ctl::Control, gtk_app: &gtk::Application) {
        use crate::ipc_ctl::Control;
        match cmd {
            Control::Activate(i) => self.activate(i),
            Control::Reveal => self.set_hidden(false),
            Control::Hide => self.set_hidden(true),
            Control::ToggleAutohide => {
                // Persist it, so the toggle survives a restart and matches
                // what the config file says.
                let mut cfg = Config::load();
                cfg.autohide.mode = match cfg.autohide.mode {
                    crate::config::HideMode::Never => crate::config::HideMode::Intelligent,
                    _ => crate::config::HideMode::Never,
                };
                let mode = cfg.autohide.mode;
                if let Err(e) = cfg.save() {
                    tracing::error!(error = %e, "cannot save autohide mode");
                }
                tracing::info!(?mode, "autohide toggled");
            }
            Control::Reload => {
                self.raw_cfg = Config::load();
                self.restyle();
                self.rebuild(gtk_app);
            }
            // Theme only: what Omarchy's theme-set hook fires. Restyling keeps
            // hover and slide state, and only rebuilds if the new theme's
            // scale actually moved the geometry.
            Control::Restyle => {
                let before = geometry_inputs(&self.cfg);
                self.restyle();
                if geometry_inputs(&self.cfg) != before {
                    self.rebuild(gtk_app);
                }
            }
        }
    }

    /// Activate the nth dock item, exactly as a left-click would.
    fn activate(&mut self, index: usize) {
        let items = self.current_items();
        let Some(item) = items.get(index) else {
            tracing::warn!(index, count = items.len(), "no such dock item");
            return;
        };

        let cmd = match item.click_target() {
            Some(addr) => Some(crate::runtime::DockCommand::Focus(addr.clone())),
            None => (!item.exec.is_empty())
                .then(|| crate::runtime::DockCommand::Exec(item.exec.clone())),
        };
        if let (Some(cmd), Some(w)) = (cmd, &self.worker) {
            let _ = w.commands.try_send(cmd);
        }
    }

    fn set_hidden(&self, hidden: bool) {
        for d in &self.docks {
            d.set_hidden(hidden, &self.cfg);
        }
    }

    /// Re-evaluate auto-hide, per surface.
    ///
    /// Each dock decides independently: a window covering one monitor's dock
    /// says nothing about the dock on another.
    fn update_autohide(&self) {
        use crate::config::HideMode;

        // Two things override the hide mode outright, because in both cases
        // the dock is in the way of something the user is deliberately
        // pointing at the screen — and `never` would otherwise pin it there.
        if self.cfg.autohide.hide_while_recording && crate::omarchy::is_recording() {
            tracing::debug!("hiding: screen recording in progress");
            self.set_hidden(true);
            return;
        }
        if self.cfg.autohide.hide_on_fullscreen && self.state.has_fullscreen() {
            tracing::debug!("hiding: a window is fullscreen");
            self.set_hidden(true);
            return;
        }

        if self.cfg.autohide.mode == HideMode::Never {
            self.set_hidden(false);
            return;
        }

        let focused = self.state.focused_client();

        for dock in &self.docks {
            // Fall back to the focused output when a surface has no name,
            // which happens only if GDK gave us no connector.
            let monitor = dock
                .monitor_name
                .as_deref()
                .and_then(|n| self.state.monitor_by_name(n))
                .or_else(|| self.state.focused_monitor());

            let Some(monitor) = monitor else { continue };
            let (w, h) = dock.panel_size;
            let rect = crate::autohide::dock_rect(&self.cfg, monitor, w, h);
            let hide = crate::autohide::should_hide(
                &self.cfg,
                &rect,
                monitor.id,
                self.state.clients(),
                focused,
            );
            dock.set_hidden(hide, &self.cfg);
        }
    }

    /// Ask the async worker to re-query the full window list.
    fn request_snapshot(&self) {
        if let Some(w) = &self.worker {
            // Full queue already means "resync pending", so dropping is right.
            let _ = w.snapshot.try_send(());
        }
    }

    /// The items currently rendered, in dock order.
    fn current_items(&self) -> Vec<DockItem> {
        self.state.items(&self.cfg)
    }

    /// Apply current state to the dock, refreshing in place when possible.
    ///
    /// Recreating layer surfaces on every focus change would flicker and reset
    /// the auto-hide slide, so a full rebuild is reserved for changes that
    /// alter the item set itself.
    fn sync(&mut self, gtk_app: &gtk::Application) {
        let items = self.current_items();
        if self.docks.is_empty() {
            self.rebuild(gtk_app);
            return;
        }

        // Cheapest first: refresh state in place, else re-order the existing
        // widgets, and only build new ones for a genuinely different item set.
        // Recreating the layer surface flickers and drops the dock for a
        // frame, which is very visible after a drag-and-drop.
        let handled = self.docks.iter().all(|d| d.refresh(&items))
            || self.docks.iter().all(|d| d.reorder(&items, &self.cfg));

        if handled {
            self.update_autohide();
        } else {
            self.rebuild(gtk_app);
        }
    }

    fn rebuild(&mut self, gtk_app: &gtk::Application) {
        let items = self.current_items();

        for d in self.docks.drain(..) {
            d.close();
        }
        let sink = make_sink(self.worker.clone());
        self.docks = build_docks(gtk_app, &self.cfg, &items, sink);
        self.update_autohide();
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
    let worker = match crate::runtime::spawn(tx) {
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
            raw_cfg: cfg.clone(),
            cfg,
            state: DockState::new(entries),
            worker: worker.clone(),
            palette: Palette::default(),
            shell: Shell::fallback(&Palette::default()),
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
                    // A recording started or stopped; nothing else changed.
                    AppEvent::HidePolicyChanged => app.update_autohide(),
                    AppEvent::StyleChanged => {
                        // A theme carries its own spacing and font scale, so
                        // switching themes can change the dock's geometry, not
                        // just its colours.
                        let before = geometry_inputs(&app.cfg);
                        app.restyle();
                        if geometry_inputs(&app.cfg) != before {
                            app.rebuild(&gtk_app);
                        }
                    }
                    AppEvent::HyprSnapshot { clients, monitors, workspaces, focused } => {
                        tracing::info!(
                            windows = clients.len(),
                            monitors = monitors.len(),
                            "state snapshot"
                        );
                        app.state.set_clients(clients);
                        app.state.set_monitors(monitors);
                        app.state.set_workspaces(workspaces);
                        app.state.set_focused(focused);
                        app.sync(&gtk_app);
                    }
                    AppEvent::Hypr(e) => app.on_hypr(e, &gtk_app),
                    AppEvent::Control(c) => app.on_control(c, &gtk_app),
                    AppEvent::ConfigChanged => {
                        let next = Config::load();
                        // Geometry-affecting changes need new surfaces;
                        // anything else is just a restyle, which is far
                        // cheaper and preserves hover state.
                        let structural = needs_rebuild(&app.raw_cfg, &next);
                        app.raw_cfg = next;
                        // restyle re-derives `cfg` from the new raw config and
                        // the current shell scale.
                        app.restyle();
                        if structural {
                            app.rebuild(&gtk_app);
                        } else {
                            // Item changes (reordering, pinning) go through
                            // sync, which reorders in place where it can
                            // rather than recreating the surface.
                            app.sync(&gtk_app);
                        }
                    }
                }
            }
        });
    });

    gtk_app.run()
}

/// Fold the active theme's shell scale into the config's pixel sizes.
///
/// The Omarchy shell multiplies every spacing and font token by
/// `spacing.scale * fontScale`, so `omarchy display text size` resizes the bar,
/// the menu and every panel at once. A dock that ignored it would be the one
/// surface that stayed put — so the same factor is applied here, to the sizes
/// the user configured rather than replacing them.
fn effective(mut cfg: Config, shell: &crate::theme::shell::Shell) -> Config {
    // ── the shell's scale ───────────────────────────────────────────────────
    // The Omarchy shell multiplies every spacing and font token by
    // `spacing.scale * fontScale`, so `omarchy display text size` resizes the
    // bar, the menu and every panel at once. A dock that ignored it would be
    // the one surface that stayed put — so the same factor is applied here, to
    // the sizes the user configured rather than replacing them.
    if cfg.theme.follow_shell_scale {
        let f = shell.metrics.spacing_factor();
        // Guard against a theme with a nonsensical scale making the dock
        // unusable, and skip the work entirely at the overwhelmingly common 1.
        if f.is_finite() && (0.25..=4.0).contains(&f) && (f - 1.0).abs() >= 0.005 {
            cfg.dock.icon_size *= f;
            cfg.dock.padding_x *= f;
            cfg.dock.padding_y *= f;
            cfg.dock.spacing = cfg.dock.spacing.map(|s| s * f);
            cfg.dock.radius *= f;
            cfg.dock.edge_offset = (cfg.dock.edge_offset as f64 * f).round() as i32;
            // Magnification lift is a pixel distance too, so it has to track
            // the icon size or a big dock barely rises and a small one leaps.
            cfg.magnify.lift *= f;
        }
    }

    // ── clearing the bar ────────────────────────────────────────────────────
    // The Omarchy bar is a layer surface too, and nothing stops the two from
    // being anchored to the same screen edge. Clear it rather than moving the
    // dock elsewhere: the user picked the dock's edge, and sitting invisibly
    // underneath the bar is the one outcome nobody wants.
    //
    // Added after scaling, because the bar's thickness is measured in final
    // pixels — it already carries the shell's font scale — and must not be
    // scaled a second time.
    if cfg.dock.avoid_bar {
        let clearance =
            crate::omarchy::bar_clearance(cfg.dock.position, crate::omarchy::bar(shell));
        if clearance > 0.0 {
            tracing::info!(clearance, position = ?cfg.dock.position, "clearing the Omarchy bar");
            cfg.dock.edge_offset += clearance.round() as i32;
        }
    }

    cfg
}

/// The parts of the config that decide surface geometry.
///
/// Used to tell whether re-deriving the config after a theme change actually
/// moved anything, so a mere recolour does not rebuild the surfaces.
fn geometry_inputs(cfg: &Config) -> [i64; 6] {
    // Fixed-point rather than floats so this can be compared for equality.
    let q = |v: f64| (v * 64.0).round() as i64;
    [
        q(cfg.dock.icon_size),
        q(cfg.dock.padding_x),
        q(cfg.dock.padding_y),
        q(cfg.dock.spacing.unwrap_or(-1.0)),
        q(cfg.dock.radius),
        cfg.dock.edge_offset as i64,
    ]
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
        // A changed *set* of pins still needs new widgets, but a reordering
        // does not — sync() reorders in place, so only length changes here.
        || old.items.pinned.len() != new.items.pinned.len()
        || old.items.show_trash != new.items.show_trash
        || old.items.glyph_ui != new.items.glyph_ui
        || old.items.commands.len() != new.items.commands.len()
        || old.workspaces.enabled != new.workspaces.enabled
        || old.workspaces.scratchpad != new.workspaces.scratchpad
        || old.workspaces.show_empty != new.workspaces.show_empty
        || old.items.folders.len() != new.items.folders.len()
        || old
            .items
            .folders
            .iter()
            .zip(&new.items.folders)
            .any(|(a, b)| a.enabled != b.enabled || a.path != b.path)
        || old.monitors.mode != new.monitors.mode
        || old.monitors.primary != new.monitors.primary
}

/// Create one surface per monitor the config asks for.
/// Apply an edit to the pinned list and persist it.
fn edit_pins<F: FnOnce(&mut Vec<String>)>(f: F) {
    let mut cfg = Config::load();
    f(&mut cfg.items.pinned);
    if let Err(e) = cfg.save() {
        tracing::error!(error = %e, "cannot save pinned list");
    }
}

/// Turn UI intent into worker commands and config edits.
///
/// Pin changes are written to `config.toml` rather than applied in memory: the
/// file watcher then reloads and rebuilds, so a pin toggled from the menu and
/// one typed into the config take exactly the same path.
fn make_sink(worker: Option<crate::runtime::Handles>) -> crate::ui::dock::ActionSink {
    std::rc::Rc::new(move |action: MenuAction| match action {
        // Straight to the shell: no worker round-trip, because this neither
        // touches dock state nor needs Hyprland.
        MenuAction::OpenSurface(s) => crate::omarchy::open(s),
        MenuAction::Command(cmd) => {
            if let Some(w) = &worker {
                if let Err(e) = w.commands.try_send(cmd) {
                    tracing::warn!(error = %e, "dropping command; worker busy");
                }
            }
        }
        MenuAction::Rescan => {
            // Cheapest correct refresh: ask for a snapshot, which rebuilds and
            // re-evaluates the Trash icon's empty/full state.
            if let Some(w) = &worker {
                let _ = w.snapshot.try_send(());
            }
        }
        MenuAction::ReorderPin { from, to } => {
            edit_pins(|pins| {
                crate::state::reorder_in_list(pins, from, to);
            });
        }
        MenuAction::RemovePin { index } => {
            edit_pins(|pins| {
                if index < pins.len() {
                    pins.remove(index);
                }
            });
        }
        MenuAction::SetPinned { key, pinned } => {
            let mut cfg = Config::load();
            cfg.items.pinned.retain(|p| p != &key);
            if pinned {
                cfg.items.pinned.push(key.clone());
            }
            match cfg.save() {
                Ok(()) => tracing::info!(%key, pinned, "pin updated"),
                Err(e) => tracing::error!(%key, error = %e, "cannot save pin"),
            }
        }
    })
}

fn build_docks(
    gtk_app: &gtk::Application,
    cfg: &Config,
    items: &[DockItem],
    sink: crate::ui::dock::ActionSink,
) -> Vec<DockSurface> {
    use crate::config::MonitorMode;

    let Some(display) = gdk::Display::default() else {
        return vec![DockSurface::build(gtk_app, cfg, items, None, sink.clone())];
    };
    let monitors = display.monitors();
    let all: Vec<gdk::Monitor> = (0..monitors.n_items())
        .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
        .collect();

    if all.is_empty() {
        return vec![DockSurface::build(gtk_app, cfg, items, None, sink.clone())];
    }

    match cfg.monitors.mode {
        MonitorMode::All => {
            all.iter().map(|m| DockSurface::build(gtk_app, cfg, items, Some(m), sink.clone())).collect()
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
            vec![DockSurface::build(gtk_app, cfg, items, Some(chosen), sink.clone())]
        }
    }
}
