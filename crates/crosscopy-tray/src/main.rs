//! CrossCopy tray app: runs the sync daemon in the background and offers
//! status, pause, pairing and start-at-login from a tray / menu bar icon.

// No console window on Windows; logs go to a file instead.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod icons;
#[cfg(windows)]
mod theme;

use anyhow::{Context, Result, bail};
use crosscopy::config::{Config, IconColor, Paths};
use crosscopy::daemon::{self, Control, SharedStatus, Status};
use crosscopy::pair;
use crosscopy_net::Identity;
use crosscopy_update::Update;
use rfd::{AsyncMessageDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};

const TITLE: &str = "CrossCopy";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How often the menu and icon are refreshed from the daemon's status.
const REFRESH: Duration = Duration::from_secs(1);
/// Logs larger than this are rotated at startup.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(10);
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

enum UserEvent {
    Menu(MenuEvent),
    PairingFinished,
    UpdateChecked { manual: bool, result: Result<Option<Update>, String> },
    UpdateDownloaded(Result<PathBuf, String>),
}

enum UpdateState {
    /// Development build: never updates itself.
    Disabled,
    Idle,
    Checking,
    Available(Update),
    Downloading,
}

fn main() {
    if let Err(e) = run() {
        error!("{e:#}");
        error_dialog(&format!("{e:#}"));
    }
}

fn run() -> Result<()> {
    let paths = Paths::new(None)?;
    init_logging(&paths.log_file)?;
    info!("starting tray app");

    let runtime = Runtime::new()?;
    let status = SharedStatus::default();
    let (control, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (daemon_done_tx, daemon_done) = std::sync::mpsc::channel();
    {
        let (paths, status) = (paths.clone(), status.clone());
        runtime.spawn(async move {
            let result = daemon::run(&paths, control_rx, status).await;
            let _ = daemon_done_tx.send(result);
        });
    }

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        // Menu bar only: no Dock icon, no app menu.
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let proxy = event_loop.create_proxy();
    {
        let proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(UserEvent::Menu(event));
        }));
    }

    let mut app = App::new(paths, runtime, status, control, daemon_done, proxy)?;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        match event {
            Event::NewEvents(StartCause::Init) => {
                // The tray icon must be created once the event loop runs.
                if let Err(e) = app.create_tray() {
                    error_dialog(&format!("Couldn't create the tray icon: {e:#}"));
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                if app.daemon_stopped() {
                    *control_flow = ControlFlow::Exit;
                } else {
                    app.refresh();
                }
            }
            Event::UserEvent(UserEvent::Menu(event)) => {
                if app.on_menu(&event.id) {
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::PairingFinished) => {
                app.pairing = false;
                app.refresh();
            }
            Event::UserEvent(UserEvent::UpdateChecked { manual, result }) => {
                app.on_update_checked(manual, result);
            }
            Event::UserEvent(UserEvent::UpdateDownloaded(result)) => {
                if app.on_update_downloaded(result) {
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    })
}

struct Items {
    status: MenuItem,
    pause: CheckMenuItem,
    pair: MenuItem,
    devices: Submenu,
    autostart: CheckMenuItem,
    update: MenuItem,
    open_log: MenuItem,
    quit: MenuItem,
}

struct App {
    paths: Paths,
    runtime: Runtime,
    status: SharedStatus,
    control: UnboundedSender<Control>,
    daemon_done: Receiver<Result<()>>,
    proxy: EventLoopProxy<UserEvent>,
    items: Items,
    menu: Option<Menu>,
    tray: Option<TrayIcon>,
    /// Status last reflected in the menu, to avoid rebuilding it every tick.
    shown: Option<Status>,
    /// (black glyph, paused) last used for the icon.
    icon_state: Option<(bool, bool)>,
    unpair_items: HashMap<MenuId, String>,
    pairing: bool,
    update: UpdateState,
    next_update_check: Instant,
    /// `tray_icon` from config, read at startup.
    icon_color: Option<IconColor>,
}

impl App {
    fn new(
        paths: Paths,
        runtime: Runtime,
        status: SharedStatus,
        control: UnboundedSender<Control>,
        daemon_done: Receiver<Result<()>>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self> {
        let items = Items {
            status: MenuItem::new("Starting…", false, None),
            pause: CheckMenuItem::new("Pause syncing", true, false, None),
            pair: MenuItem::new("Pair new device…", true, None),
            devices: Submenu::new("Paired devices", true),
            autostart: CheckMenuItem::new("Start at login", true, autostart::is_enabled(), None),
            update: MenuItem::new("", false, None),
            open_log: MenuItem::new("Open log", true, None),
            quit: MenuItem::new("Quit CrossCopy", true, None),
        };
        let menu = Menu::new();
        menu.append_items(&[
            &items.status,
            &PredefinedMenuItem::separator(),
            &items.pause,
            &items.pair,
            &items.devices,
            &PredefinedMenuItem::separator(),
            &items.autostart,
            &items.update,
            &items.open_log,
            &PredefinedMenuItem::separator(),
            &items.quit,
        ])?;
        Ok(Self {
            runtime,
            status,
            control,
            daemon_done,
            proxy,
            items,
            menu: Some(menu),
            tray: None,
            shown: None,
            icon_state: None,
            unpair_items: HashMap::new(),
            pairing: false,
            update: if crosscopy_update::enabled() { UpdateState::Idle } else { UpdateState::Disabled },
            next_update_check: Instant::now() + FIRST_UPDATE_CHECK,
            icon_color: Config::load(&paths.config_file).ok().and_then(|c| c.tray_icon),
            paths,
        })
    }

    fn create_tray(&mut self) -> Result<()> {
        let menu = self.menu.take().context("tray already created")?;
        let black = wants_black_glyph(self.icon_color);
        let icon = icons::tray(black, false);
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .with_tooltip(TITLE);
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon);
        self.tray = Some(builder.build()?);
        self.icon_state = Some((black, false));
        self.refresh();
        Ok(())
    }

    /// Returns true (after telling the user why) if the daemon has exited.
    fn daemon_stopped(&self) -> bool {
        match self.daemon_done.try_recv() {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                error!("sync stopped: {e:#}");
                let message = if format!("{e:#}").contains("binding UDP") {
                    "CrossCopy is already running (check the tray), or `crosscopy run` is running in a terminal.".to_owned()
                } else {
                    format!("Syncing stopped because of an error:\n\n{e:#}")
                };
                error_dialog(&message);
                true
            }
            Err(_) => false,
        }
    }

    fn refresh(&mut self) {
        let status = self.status.lock().unwrap().clone();
        let mut text = if self.pairing {
            "Pairing… start pairing on the other device".to_owned()
        } else if !status.running {
            "Starting…".to_owned()
        } else if status.paired.is_empty() {
            "No devices paired yet".to_owned()
        } else if status.connected.is_empty() {
            format!("Waiting for {}", status.paired.join(", "))
        } else {
            format!("Connected to {}", status.connected.join(", "))
        };
        if status.paused {
            text = format!("Paused · {text}");
        }
        self.items.status.set_text(&text);
        self.items.pair.set_enabled(!self.pairing);

        if matches!(self.update, UpdateState::Idle) && Instant::now() >= self.next_update_check {
            self.next_update_check = Instant::now() + UPDATE_CHECK_INTERVAL;
            self.start_update_check(false);
        }
        self.refresh_update_item();
        if matches!(self.update, UpdateState::Available(_)) {
            text = format!("{text} · Update available");
        }

        if self.shown.as_ref().is_none_or(|s| s.paired != status.paired || s.connected != status.connected) {
            self.rebuild_devices(&status);
        }
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(format!("{TITLE} — {text}")));
            let state = (wants_black_glyph(self.icon_color), status.paused);
            if self.icon_state != Some(state) {
                let icon = Some(icons::tray(state.0, state.1));
                #[cfg(target_os = "macos")]
                let _ = tray.set_icon_templated(icon);
                #[cfg(not(target_os = "macos"))]
                let _ = tray.set_icon(icon);
                self.icon_state = Some(state);
            }
        }
        self.shown = Some(status);
    }

    fn rebuild_devices(&mut self, status: &Status) {
        while self.items.devices.remove_at(0).is_some() {}
        self.unpair_items.clear();
        if status.paired.is_empty() {
            let _ = self.items.devices.append(&MenuItem::new("None yet", false, None));
            return;
        }
        for name in &status.paired {
            let label = if status.connected.contains(name) {
                format!("{name} — connected")
            } else {
                name.clone()
            };
            let device = Submenu::new(label, true);
            let unpair = MenuItem::new("Unpair…", true, None);
            self.unpair_items.insert(unpair.id().clone(), name.clone());
            let _ = device.append(&unpair);
            let _ = self.items.devices.append(&device);
        }
    }

    /// Handles a menu click; returns true when the app should exit.
    fn on_menu(&mut self, id: &MenuId) -> bool {
        if id == self.items.pause.id() {
            let _ = self.control.send(Control::SetPaused(self.items.pause.is_checked()));
        } else if id == self.items.pair.id() {
            self.start_pairing();
        } else if id == self.items.autostart.id() {
            let enabled = self.items.autostart.is_checked();
            if let Err(e) = autostart::set(enabled) {
                self.items.autostart.set_checked(!enabled);
                error_dialog(&format!("{e:#}"));
            }
        } else if id == self.items.update.id() {
            self.on_update_clicked();
        } else if id == self.items.open_log.id() {
            open_file(&self.paths.log_file);
        } else if id == self.items.quit.id() {
            self.quit();
            return true;
        } else if let Some(name) = self.unpair_items.get(id).cloned() {
            self.unpair(&name);
        }
        false
    }

    fn start_pairing(&mut self) {
        if self.pairing {
            return;
        }
        self.pairing = true;
        self.refresh();
        let (paths, control, proxy) = (self.paths.clone(), self.control.clone(), self.proxy.clone());
        self.runtime.spawn(async move {
            match pair_flow(&paths, &control).await {
                Ok(Some(name)) => {
                    info!("paired with {name}");
                    dialog(MessageLevel::Info, &format!("Paired with \"{name}\". Clipboards will now sync.")).await;
                }
                Ok(None) => info!("pairing cancelled"),
                Err(e) => {
                    warn!("pairing failed: {e:#}");
                    dialog(MessageLevel::Warning, &format!("Pairing didn't complete.\n\n{e:#}")).await;
                }
            }
            let _ = proxy.send_event(UserEvent::PairingFinished);
        });
    }

    fn unpair(&mut self, name: &str) {
        let confirmed = MessageDialog::new()
            .set_level(MessageLevel::Warning)
            .set_title(TITLE)
            .set_description(format!(
                "Unpair \"{name}\"?\n\nClipboards stop syncing with it until you pair again."
            ))
            .set_buttons(MessageButtons::YesNo)
            .show();
        if confirmed != MessageDialogResult::Yes {
            return;
        }
        let result = Config::load(&self.paths.config_file).and_then(|mut config| {
            config.remove_peer(name);
            config.save(&self.paths.config_file)
        });
        match result {
            Ok(()) => {
                info!("unpaired {name}");
                let _ = self.control.send(Control::ReloadConfig);
            }
            Err(e) => error_dialog(&format!("Couldn't unpair \"{name}\": {e:#}")),
        }
    }

    fn refresh_update_item(&self) {
        let (text, enabled) = match &self.update {
            UpdateState::Disabled => (format!("CrossCopy {VERSION} (development build)"), false),
            UpdateState::Idle => (format!("Check for updates (v{VERSION})"), true),
            UpdateState::Checking => ("Checking for updates…".to_owned(), false),
            UpdateState::Available(update) => (format!("Install update v{}…", update.version), true),
            UpdateState::Downloading => ("Downloading update…".to_owned(), false),
        };
        self.items.update.set_text(text);
        self.items.update.set_enabled(enabled);
    }

    fn start_update_check(&mut self, manual: bool) {
        self.update = UpdateState::Checking;
        self.refresh_update_item();
        let proxy = self.proxy.clone();
        self.runtime.spawn_blocking(move || {
            let result = crosscopy_update::check(VERSION).map_err(|e| format!("{e:#}"));
            let _ = proxy.send_event(UserEvent::UpdateChecked { manual, result });
        });
    }

    fn on_update_checked(&mut self, manual: bool, result: Result<Option<Update>, String>) {
        self.update = UpdateState::Idle;
        match result {
            Ok(Some(update)) => {
                info!(version = %update.version, "update available");
                self.update = UpdateState::Available(update);
            }
            Ok(None) if manual => info_dialog(&format!("CrossCopy {VERSION} is up to date.")),
            Ok(None) => {}
            Err(e) => {
                warn!("update check failed: {e}");
                if manual {
                    error_dialog(&format!("Couldn't check for updates.\n\n{e}"));
                }
            }
        }
        self.refresh();
    }

    fn on_update_clicked(&mut self) {
        match &self.update {
            UpdateState::Idle => self.start_update_check(true),
            UpdateState::Available(update) => {
                let notes = if update.notes.is_empty() { String::new() } else { format!("\n\n{}", update.notes) };
                let confirmed = MessageDialog::new()
                    .set_level(MessageLevel::Info)
                    .set_title(TITLE)
                    .set_description(format!(
                        "Update CrossCopy from {VERSION} to {}?{notes}\n\nCrossCopy will restart.",
                        update.version
                    ))
                    .set_buttons(MessageButtons::YesNo)
                    .show();
                if confirmed != MessageDialogResult::Yes {
                    return;
                }
                let update = update.clone();
                self.update = UpdateState::Downloading;
                self.refresh_update_item();
                let proxy = self.proxy.clone();
                self.runtime.spawn_blocking(move || {
                    let dir = std::env::temp_dir().join("crosscopy-update");
                    let result = crosscopy_update::download(&update, &dir).map_err(|e| format!("{e:#}"));
                    let _ = proxy.send_event(UserEvent::UpdateDownloaded(result));
                });
            }
            _ => {}
        }
    }

    /// Installs a downloaded update; returns true when the app should exit
    /// so the new version can start.
    fn on_update_downloaded(&mut self, result: Result<PathBuf, String>) -> bool {
        let installed = result.and_then(|path| crosscopy_update::install(&path).map_err(|e| format!("{e:#}")));
        match installed {
            Ok(()) => {
                info!("update installed; restarting");
                self.quit();
                true
            }
            Err(e) => {
                error!("update failed: {e}");
                error_dialog(&format!("The update couldn't be installed.\n\n{e}"));
                self.update = UpdateState::Idle;
                self.refresh();
                false
            }
        }
    }

    fn quit(&mut self) {
        info!("quitting");
        let _ = self.control.send(Control::Shutdown);
        // Give the daemon a moment to say goodbye to peers and on mDNS.
        let _ = self.daemon_done.recv_timeout(Duration::from_secs(2));
        self.tray.take();
    }
}

/// Runs one pairing attempt; returns the saved name, or None if declined here.
async fn pair_flow(paths: &Paths, control: &UnboundedSender<Control>) -> Result<Option<String>> {
    let config = Config::load(&paths.config_file)?;
    let identity = Identity::load_or_create(&paths.identity_dir)?;
    let (_pairing, pending) = pair::find(&identity, &config.device_name(), None, |_, _| {}).await?;

    let (id, name, ip) = (pending.peer_id, pending.peer_name.clone(), pending.peer_addr.ip());
    let answer = AsyncMessageDialog::new()
        .set_level(MessageLevel::Info)
        .set_title(format!("Pair with \"{name}\""))
        .set_description(format!(
            "Check that \"{name}\" shows the same code:\n\n{}\n\nPair these devices?",
            pending.code
        ))
        .set_buttons(MessageButtons::YesNo)
        .show()
        .await;
    if answer != MessageDialogResult::Yes {
        let _ = pending.finish(false).await;
        return Ok(None);
    }
    if !pending.finish(true).await? {
        bail!("\"{name}\" declined the pairing.");
    }
    let saved_as = pair::save(paths, id, &name, ip)?;
    let _ = control.send(Control::ReloadConfig);
    Ok(Some(saved_as))
}

/// On macOS the icon is a template image that the system tints; Windows
/// follows the taskbar theme; on Linux (Waybar etc.) the bar's colors can't
/// be detected, so it's white unless config says `tray_icon = "black"`.
fn wants_black_glyph(linux_color: Option<IconColor>) -> bool {
    #[cfg(windows)]
    let _ = linux_color;
    #[cfg(windows)]
    return theme::taskbar_is_light();
    #[cfg(target_os = "macos")]
    let _ = linux_color;
    #[cfg(target_os = "macos")]
    return true;
    #[cfg(not(any(windows, target_os = "macos")))]
    return linux_color == Some(IconColor::Black);
}

async fn dialog(level: MessageLevel, message: &str) {
    AsyncMessageDialog::new()
        .set_level(level)
        .set_title(TITLE)
        .set_description(message)
        .set_buttons(MessageButtons::Ok)
        .show()
        .await;
}

fn info_dialog(message: &str) {
    MessageDialog::new()
        .set_level(MessageLevel::Info)
        .set_title(TITLE)
        .set_description(message)
        .set_buttons(MessageButtons::Ok)
        .show();
}

fn error_dialog(message: &str) {
    MessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title(TITLE)
        .set_description(message)
        .set_buttons(MessageButtons::Ok)
        .show();
}

fn open_file(path: &Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(path).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = result {
        warn!("couldn't open {}: {e}", path.display());
    }
}

fn init_logging(log_file: &Path) -> Result<()> {
    if let Some(dir) = log_file.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if fs::metadata(log_file).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::rename(log_file, log_file.with_extension("log.old"));
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .with_context(|| format!("opening {}", log_file.display()))?;
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn,rustls=warn,mdns_sd=warn".into()),
        )
        .init();
    std::panic::set_hook(Box::new(|info| error!("panic: {info}")));
    Ok(())
}
