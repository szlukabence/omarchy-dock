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
use crate::state::{DockItem, ItemKind};
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
    /// For workers started on the main thread, like the drive watcher.
    tx: crate::event::Sender,
    /// Watches removable drives while they are shown.
    drives: Option<crate::drives::DriveWatcher>,
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

    /// Watch for drives only while the dock shows them.
    fn sync_drive_watcher(&mut self) {
        match (self.cfg.items.show_drives, self.drives.is_some()) {
            (true, false) => {
                self.drives = Some(crate::drives::DriveWatcher::start(self.tx.clone()))
            }
            (false, true) => {
                self.drives = None;
                self.state.set_drives(Vec::new());
            }
            _ => {}
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
                let _ = self.state.set_urgent(addr);
                true
            }
            // The snapshot carries each client's fullscreen state, and the
            // event only says that *something* changed.
            Fullscreen(_) => {
                self.request_snapshot();
                false
            }
            WindowTitle { addr, title } => {
                // Titles show in the window list and the previews. The key
                // sequence does not change, so this is a cheap in-place
                // refresh, never a rebuild.
                self.state.set_title(&addr, title);
                true
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
            FocusedMonitor(name) => {
                self.state.set_focused_monitor(&name);
                self.follow_focused_monitor(gtk_app);
                false
            }
            _ => false,
        };

        if dirty {
            self.sync(gtk_app);
        }
    }

    /// In `focused` mode, move the dock to the focused monitor when it is not
    /// already there. Moving means a new surface, since a layer surface is
    /// bound to its output for life. Returns whether it moved.
    fn follow_focused_monitor(&mut self, gtk_app: &gtk::Application) -> bool {
        if self.cfg.monitors.mode != crate::config::MonitorMode::Focused {
            return false;
        }
        let Some(focused) = self.state.focused_monitor().map(|m| m.name.clone()) else {
            return false;
        };
        let here = self.docks.first().and_then(|d| d.monitor_name.clone());
        if here.as_deref() == Some(focused.as_str()) {
            return false;
        }
        // A monitor GTK does not know (yet) would only rebuild onto the
        // fallback; wait for the snapshot that follows it being added.
        let known = gdk::Display::default().is_some_and(|d| {
            let ms = d.monitors();
            (0..ms.n_items()).any(|i| {
                ms.item(i)
                    .and_downcast::<gdk::Monitor>()
                    .and_then(|m| m.connector())
                    .is_some_and(|c| c == focused)
            })
        });
        if !known {
            return false;
        }
        tracing::debug!(from = ?here, to = %focused, "following the focused monitor");
        self.rebuild(gtk_app);
        true
    }

    /// Handle a command from `omarchy-dockctl`.
    fn on_control(&mut self, cmd: crate::ipc_ctl::Control, gtk_app: &gtk::Application) {
        use crate::ipc_ctl::Control;
        match cmd {
            Control::Activate(i) => self.activate(i),
            Control::Minimize => {
                let cmd = self
                    .state
                    .focused_client()
                    .filter(|c| !c.is_minimized())
                    .map(|c| {
                        crate::runtime::DockCommand::minimize(
                            &c.address,
                            &crate::state::WindowMeta::of(c),
                        )
                    });
                self.send(cmd);
            }
            Control::Restore => {
                let cmd = self.state.last_minimized().map(|c| {
                    crate::runtime::DockCommand::restore(&c.address, &crate::state::WindowMeta::of(c))
                });
                self.send(cmd);
            }
            Control::Reveal => self.set_hidden(false),
            Control::Hide => self.set_hidden(true),
            Control::ToggleAutohide => {
                // Persist it, so the toggle survives a restart and matches
                // what the config file says.
                let mut mode = None;
                let saved = Config::edit(|cfg| {
                    cfg.autohide.mode = match cfg.autohide.mode {
                        crate::config::HideMode::Never => crate::config::HideMode::Intelligent,
                        _ => crate::config::HideMode::Never,
                    };
                    mode = Some(cfg.autohide.mode);
                });
                match saved {
                    Ok(()) => tracing::info!(?mode, "autohide toggled"),
                    Err(e) => tracing::error!(error = %e, "cannot save autohide mode"),
                }
            }
            Control::Reload => {
                self.raw_cfg = Config::load();
                self.restyle();
                self.rebuild(gtk_app);
            }
            Control::Settings => crate::ui::settings::open_window(),
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
        let Some(item) = crate::state::nth_app(&items, index) else {
            tracing::warn!(index, "no such dock app");
            return;
        };

        let cmd = crate::runtime::DockCommand::for_click(item);
        self.send(cmd);
    }

    /// Hand a command to the worker, if there is one to hand.
    fn send(&self, cmd: Option<crate::runtime::DockCommand>) {
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

        // The screensaver outranks everything below, hover included: nothing
        // should come up over it.
        let away = self.state.screensaver_showing();
        for d in &self.docks {
            d.set_away(away, &self.cfg);
        }
        if away {
            tracing::debug!("away: the screensaver is up");
            return;
        }

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
    ///
    /// Also where unread counts are cleared: an app whose window has focus
    /// has been looked at, and this runs after every change that could have
    /// focused one.
    fn current_items(&mut self) -> Vec<DockItem> {
        let items = self.state.items(&self.cfg);
        if self.state.clear_seen(&items) {
            return self.state.items(&self.cfg);
        }
        items
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
            || self.docks.iter().all(|d| d.reorder(&items));

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
        let focused = self.state.focused_monitor().map(|m| m.name.clone());
        self.docks = build_docks(gtk_app, &self.cfg, &items, focused.as_deref(), sink);
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
    watch_signals(tx.clone());
    if let Err(e) = crate::config::watcher::spawn(tx.clone()) {
        tracing::error!(error = %e, "live reload unavailable");
    }
    // Read once here rather than reacting to the setting later: hosting the
    // tray means claiming a bus name and registering with the watcher, which
    // is a process-lifetime thing, not something to toggle per frame.
    let startup = Config::load();
    let (tray, media) = (startup.tray.enabled, startup.items.media_controls);
    let badges = startup.items.notification_badges;
    let drive_tx = tx.clone();
    let worker = match crate::runtime::spawn(tx, tray, media, badges) {
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
        sync_bar_workspaces(&cfg);
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
            tx: drive_tx.clone(),
            drives: None,
        };
        app.state.set_recording(crate::omarchy::is_recording());
        app.sync_drive_watcher();
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
                    // An application was installed or removed. Rare, and it
                    // can change icons and labels, which a refresh never
                    // touches — so rebuild outright.
                    AppEvent::DesktopEntriesChanged => {
                        let entries = crate::desktop::scan();
                        tracing::info!(count = entries.len(), "desktop entries rescanned");
                        app.state.set_entries(entries);
                        app.rebuild(&gtk_app);
                    }
                    // A recording started or stopped; nothing else changed.
                    AppEvent::HidePolicyChanged => {
                        app.state.set_recording(crate::omarchy::is_recording());
                        app.sync(&gtk_app);
                    }
                    AppEvent::Media(players) => {
                        app.state.set_media(players);
                        app.sync(&gtk_app);
                    }
                    AppEvent::Downloads(n) => {
                        let finished = n < app.state.downloads();
                        app.state.set_downloads(n);
                        app.sync(&gtk_app);
                        // A download landing gets one breath on its stack,
                        // the same "done" the dock gives a launch.
                        if finished {
                            if let Some(dir) = dirs::download_dir() {
                                let key = format!("__folder:{}", dir.display());
                                for d in &app.docks {
                                    d.pulse_key(&key);
                                }
                            }
                        }
                    }
                    AppEvent::Notified(notice) => {
                        if app.cfg.items.notification_badges {
                            let items = app.current_items();
                            if app.state.note_notice(&items, &notice) {
                                app.sync(&gtk_app);
                            }
                        }
                    }
                    AppEvent::Tray(items) => {
                        app.state.set_tray(items);
                        app.sync(&gtk_app);
                    }
                    AppEvent::Drives(drives) => {
                        app.state.set_drives(drives);
                        app.sync(&gtk_app);
                    }
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
                        if !app.follow_focused_monitor(&gtk_app) {
                            app.sync(&gtk_app);
                        }
                    }
                    AppEvent::Hypr(e) => app.on_hypr(e, &gtk_app),
                    AppEvent::Control(c) => app.on_control(c, &gtk_app),
                    AppEvent::Quit => gtk_app.quit(),
                    AppEvent::ConfigChanged => {
                        let next = Config::load();
                        sync_bar_workspaces(&next);
                        // Starts or stops watching notifications with it.
                        if let Some(w) = &app.worker {
                            let on = next.items.notification_badges;
                            w.badges.send_if_modified(|v| std::mem::replace(v, on) != on);
                        }
                        // Geometry-affecting changes need new surfaces;
                        // anything else is just a restyle, which is far
                        // cheaper and preserves hover state.
                        let structural = needs_rebuild(&app.raw_cfg, &next);
                        app.raw_cfg = next;
                        // restyle re-derives `cfg` from the new raw config and
                        // the current shell scale.
                        app.restyle();
                        app.sync_drive_watcher();
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

    // The bar's workspaces are hidden only while the dock is there to show
    // its own. Stopping the dock — disabling the plugin, logging out, a
    // `pkill` — sends a signal, so every way out puts them back; the next
    // start takes them out again if the setting is still on.
    gtk_app.connect_shutdown(|_| {
        if let Err(e) = crate::bar_widgets::sync(crate::bar_widgets::WORKSPACES, false) {
            tracing::warn!(error = %e, "cannot give the bar back its workspaces");
        }
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
            cfg.scale_geometry(f);
            cfg.dock.edge_offset = (cfg.dock.edge_offset as f64 * f).round() as i32;
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
        || old.tray.enabled != new.tray.enabled
        || old.tray.show_passive != new.tray.show_passive
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
    if let Err(e) = Config::edit(|cfg| f(&mut cfg.items.pinned)) {
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
        // Main thread, straight to GIO: udisks2 does the work, as the user.
        MenuAction::DriveOpen(id) => crate::drives::open(&id),
        MenuAction::DriveEject(id) => crate::drives::eject(&id),
        MenuAction::SetPinned { key, pinned } => {
            let saved = Config::edit(|cfg| {
                cfg.items.pinned.retain(|p| p != &key);
                if pinned {
                    cfg.items.pinned.push(key.clone());
                }
            });
            match saved {
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
    focused: Option<&str>,
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

    // Each dock fits its own monitor: with a dock on every screen, a laptop
    // panel and an external display have different room to offer.
    let kinds: Vec<ItemKind> = items.iter().map(|i| i.kind).collect();
    let fitted = |m: &gdk::Monitor| {
        let vertical = cfg.dock.position.is_vertical();
        // The same breathing room at the ends as between the dock and its
        // screen edge, so a full dock still looks placed rather than jammed.
        let margin = (cfg.dock.edge_offset.max(8)) as f64;
        let room = crate::ui::dock::usable_span(Some(m), vertical) - 2.0 * margin;
        fit_to(cfg, &kinds, room)
    };

    match cfg.monitors.mode {
        MonitorMode::All => all
            .iter()
            .map(|m| DockSurface::build(gtk_app, &fitted(m), items, Some(m), sink.clone()))
            .collect(),
        MonitorMode::Primary | MonitorMode::Focused => {
            let connectors: Vec<Option<String>> =
                all.iter().map(|m| m.connector().map(|c| c.to_string())).collect();
            let chosen =
                &all[choose_monitor(cfg.monitors.mode, &cfg.monitors.primary, focused, &connectors)];
            vec![DockSurface::build(gtk_app, &fitted(chosen), items, Some(chosen), sink.clone())]
        }
    }
}

/// Turn SIGTERM, SIGINT and SIGHUP into an orderly quit, so shutdown work
/// (giving the bar back its workspaces) runs however the dock is stopped.
///
/// Catching a signal takes away its default of ending the process, so this
/// keeps that promise itself: a second signal, or the quit not finishing
/// within a few seconds, ends the dock regardless. `pkill` always works.
fn watch_signals(tx: crate::event::Sender) {
    let spawned = std::thread::Builder::new().name("omarchy-dock-signals".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        rt.block_on(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
                signal(SignalKind::hangup()),
            ) else {
                return;
            };
            let next = async {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                    _ = hup.recv() => {}
                }
            };
            next.await;
            tracing::info!("asked to stop");
            let _ = tx.send(AppEvent::Quit).await;
            let again = async {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                    _ = hup.recv() => {}
                }
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), again).await;
            tracing::warn!("did not stop in time; exiting");
            std::process::exit(1);
        });
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot watch for stop signals");
    }
}

/// Hide the bar's workspace widget while the dock shows workspaces, or put it
/// back. Off the GTK thread: it talks to the shell, which takes a moment.
fn sync_bar_workspaces(cfg: &Config) {
    let hide = cfg.workspaces.enabled && cfg.workspaces.hide_bar_workspaces;
    std::thread::spawn(move || {
        if let Err(e) = crate::bar_widgets::sync(crate::bar_widgets::WORKSPACES, hide) {
            tracing::warn!(error = %e, "cannot sync the bar's workspaces");
        }
    });
}

/// Smallest the dock shrinks to, as a fraction of its configured size. Below
/// this, icons get too small to aim at comfortably.
const MIN_FIT: f64 = 0.75;

/// `cfg` with the dock shrunk just enough to fit `room` logical pixels along
/// its axis — never below [`MIN_FIT`] of its size, and never grown.
///
/// Scaling is not quite proportional, since dividers keep their width, so the
/// factor is refined against the real geometry a few times rather than
/// computed once.
fn fit_to(cfg: &Config, kinds: &[ItemKind], room: f64) -> Config {
    let length = |c: &Config| {
        let g = crate::ui::Geometry::compute(c, kinds);
        if c.dock.position.is_vertical() { g.window_h } else { g.window_w }
    };
    let natural = length(cfg);
    if room <= 0.0 || natural <= room {
        return cfg.clone();
    }
    let mut f = 1.0;
    let mut fitted = cfg.clone();
    for _ in 0..4 {
        let len = length(&fitted);
        if len <= room || f <= MIN_FIT {
            break;
        }
        f = (f * room / len).max(MIN_FIT);
        fitted = cfg.clone();
        fitted.scale_geometry(f);
    }
    tracing::info!(natural, room, factor = f, "shrinking the dock to fit");
    fitted
}

/// Which of `connectors` a single dock goes on.
///
/// `focused` follows Hyprland's focused monitor, falling back to the primary
/// when that monitor is unknown; `primary` is the configured output, falling
/// back to the first.
fn choose_monitor(
    mode: crate::config::MonitorMode,
    primary: &str,
    focused: Option<&str>,
    connectors: &[Option<String>],
) -> usize {
    let find = |name: &str| connectors.iter().position(|c| c.as_deref() == Some(name));
    let primary = (!primary.is_empty()).then(|| find(primary)).flatten();
    match mode {
        crate::config::MonitorMode::Focused => focused.and_then(find).or(primary).unwrap_or(0),
        _ => primary.unwrap_or(0),
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::choose_monitor;
    use crate::config::MonitorMode::{Focused, Primary};

    fn outputs() -> Vec<Option<String>> {
        vec![Some("eDP-1".into()), Some("DP-2".into()), Some("HDMI-A-1".into())]
    }

    #[test]
    fn focused_mode_goes_where_focus_is() {
        assert_eq!(choose_monitor(Focused, "", Some("DP-2"), &outputs()), 1);
        assert_eq!(choose_monitor(Focused, "eDP-1", Some("HDMI-A-1"), &outputs()), 2);
    }

    #[test]
    fn focused_mode_falls_back_to_the_primary_then_the_first() {
        // Focus on a monitor GTK has not seen yet, or none reported at all.
        assert_eq!(choose_monitor(Focused, "DP-2", Some("DP-9"), &outputs()), 1);
        assert_eq!(choose_monitor(Focused, "DP-2", None, &outputs()), 1);
        assert_eq!(choose_monitor(Focused, "", None, &outputs()), 0);
    }

    #[test]
    fn primary_mode_ignores_focus() {
        assert_eq!(choose_monitor(Primary, "HDMI-A-1", Some("DP-2"), &outputs()), 2);
        assert_eq!(choose_monitor(Primary, "missing", Some("DP-2"), &outputs()), 0);
    }
}

#[cfg(test)]
mod fit_tests {
    use super::{fit_to, MIN_FIT};
    use crate::config::Config;
    use crate::state::ItemKind;
    use crate::ui::Geometry;

    fn width(cfg: &Config, kinds: &[ItemKind]) -> f64 {
        Geometry::compute(cfg, kinds).window_w
    }

    fn apps(n: usize) -> Vec<ItemKind> {
        // A dock like this one: apps in groups with dividers between.
        (0..n).map(|i| if i % 6 == 5 { ItemKind::Separator } else { ItemKind::App }).collect()
    }

    #[test]
    fn a_dock_that_fits_is_left_alone() {
        let cfg = Config::default();
        let kinds = apps(8);
        let fitted = fit_to(&cfg, &kinds, width(&cfg, &kinds) + 50.0);
        assert_eq!(fitted.dock.icon_size, cfg.dock.icon_size);
    }

    #[test]
    fn a_crowded_dock_shrinks_just_enough_to_fit() {
        let cfg = Config::default();
        let kinds = apps(30);
        let room = width(&cfg, &kinds) * 0.9;
        let fitted = fit_to(&cfg, &kinds, room);
        let w = width(&fitted, &kinds);
        assert!(w <= room + 0.5, "{w} > {room}");
        // "Just enough": not shrunk far past what was needed.
        assert!(w >= room * 0.97, "{w} is well under {room}");
        assert!(fitted.dock.icon_size < cfg.dock.icon_size);
    }

    #[test]
    fn it_never_shrinks_past_the_floor() {
        let cfg = Config::default();
        let kinds = apps(30);
        let fitted = fit_to(&cfg, &kinds, 100.0);
        let floor = cfg.dock.icon_size * MIN_FIT;
        assert!((fitted.dock.icon_size - floor).abs() < 1e-9, "{}", fitted.dock.icon_size);
    }

    #[test]
    fn spacing_and_padding_shrink_with_the_icons() {
        let mut cfg = Config::default();
        cfg.dock.spacing = Some(16.0);
        let kinds = apps(30);
        let fitted = fit_to(&cfg, &kinds, width(&cfg, &kinds) * 0.85);
        let f = fitted.dock.icon_size / cfg.dock.icon_size;
        assert!((fitted.dock.spacing.unwrap() - 16.0 * f).abs() < 1e-9);
        assert!((fitted.dock.padding_x - cfg.dock.padding_x * f).abs() < 1e-9);
    }
}
