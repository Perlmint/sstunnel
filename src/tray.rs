//! macOS menu bar item / Windows tray icon: one submenu per SSH host, one
//! checkbox per forward.
//!
//! The tray is just another IPC client. A worker thread polls the daemon and
//! runs requests; the main thread owns the UI and only ever renders reports.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::sync::mpsc;
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::cli::ServiceAction;
use crate::ipc::protocol::{
    ForwardReport, ForwardState, HostReport, HostState, Request, Response, Selection, StatusReport,
};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

enum UserEvent {
    /// A fresh status, or why there is none (usually: the daemon is not running).
    Status(Result<StatusReport, String>),
    /// A request the user triggered was refused.
    ActionFailed(String),
    Menu(MenuEvent),
    TrayClicked,
}

enum Job {
    Refresh,
    Request(Request),
    StartService,
}

/// What a menu item does when clicked, encoded in its id.
enum Action {
    Toggle(String),
    HostEnable(String, bool),
    HostRestart(String),
    Reload,
    StartService,
    Quit,
}

impl Action {
    fn id(&self) -> String {
        match self {
            Action::Toggle(name) => format!("toggle:{name}"),
            Action::HostEnable(host, true) => format!("host-on:{host}"),
            Action::HostEnable(host, false) => format!("host-off:{host}"),
            Action::HostRestart(host) => format!("host-restart:{host}"),
            Action::Reload => "reload".into(),
            Action::StartService => "start-service".into(),
            Action::Quit => "quit".into(),
        }
    }

    fn parse(id: &str) -> Option<Self> {
        let (kind, arg) = id.split_once(':').unwrap_or((id, ""));
        let arg = arg.to_owned();
        Some(match kind {
            "toggle" => Action::Toggle(arg),
            "host-on" => Action::HostEnable(arg, true),
            "host-off" => Action::HostEnable(arg, false),
            "host-restart" => Action::HostRestart(arg),
            "reload" => Action::Reload,
            "start-service" => Action::StartService,
            "quit" => Action::Quit,
            _ => return None,
        })
    }
}

pub fn run(config_path: PathBuf, endpoint: String) -> Result<()> {
    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        // Menu bar only: no Dock icon, no app menu.
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));
    let click_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        // Windows reports both press and release; refresh once.
        if matches!(
            event,
            TrayIconEvent::Click {
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = click_proxy.send_event(UserEvent::TrayClicked);
        }
    }));

    let (jobs, job_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    std::thread::Builder::new()
        .name("ssh-tunnel-tray".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    let _ = ready_tx.send(Ok(()));
                    runtime.block_on(worker(endpoint, config_path, job_rx, proxy));
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            }
        })
        .context("spawning the tray worker")?;
    ready_rx
        .recv()
        .context("the tray worker exited early")?
        .context("starting the async runtime")?;

    let mut tray: Option<Tray> = None;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // The platform is only ready to host a tray item once the loop runs.
            Event::NewEvents(StartCause::Init) => match Tray::new() {
                Ok(created) => tray = Some(created),
                Err(err) => {
                    eprintln!("error: {err:#}");
                    *control_flow = ControlFlow::Exit;
                }
            },
            Event::UserEvent(event) => {
                let Some(tray) = tray.as_mut() else { return };
                match event {
                    UserEvent::Status(status) => tray.show(status),
                    UserEvent::ActionFailed(message) => tray.show_error(message),
                    UserEvent::TrayClicked => {
                        let _ = jobs.send(Job::Refresh);
                    }
                    UserEvent::Menu(event) => match Action::parse(&event.id.0) {
                        Some(Action::Quit) => *control_flow = ControlFlow::Exit,
                        Some(action) => {
                            if let Some(job) = tray.job_for(action) {
                                tray.error = None;
                                let _ = jobs.send(job);
                            }
                        }
                        None => {}
                    },
                }
            }
            _ => {}
        }
    });
}

/// Poll on a timer, and run whatever the menu asks for in between.
async fn worker(
    endpoint: String,
    config_path: PathBuf,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    proxy: EventLoopProxy<UserEvent>,
) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            job = jobs.recv() => {
                let failure = match job {
                    None => return,
                    Some(Job::Refresh) => None,
                    Some(Job::Request(request)) => {
                        match crate::ipc::client::request(&endpoint, &request).await {
                            Ok(Response::Error { message }) => Some(message),
                            Ok(_) => None,
                            Err(err) => Some(format!("{err:#}")),
                        }
                    }
                    Some(Job::StartService) => {
                        let path = config_path.clone();
                        let started = tokio::task::spawn_blocking(move || {
                            crate::service::run(&ServiceAction::Start, &path)
                        })
                        .await;
                        match started {
                            Ok(Ok(())) => {
                                // Give the service manager a moment before the status poll.
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                None
                            }
                            Ok(Err(err)) => Some(format!("{err:#}")),
                            Err(err) => Some(err.to_string()),
                        }
                    }
                };
                if let Some(message) = failure
                    && proxy.send_event(UserEvent::ActionFailed(message)).is_err()
                {
                    return;
                }
                ticker.reset();
            }
        }

        let status = match crate::ipc::client::request(&endpoint, &Request::Status).await {
            Ok(Response::Status(report)) => Ok(*report),
            Ok(Response::Error { message }) => Err(message),
            Ok(Response::Ok { message }) => Err(format!("unexpected reply: {message}")),
            Err(_) => Err("Daemon not running".into()),
        };
        if proxy.send_event(UserEvent::Status(status)).is_err() {
            return;
        }
    }
}

/// The menu's shape. Items are only rebuilt when this changes, so an open menu
/// keeps its items while their text and checkmarks update in place.
#[derive(PartialEq)]
struct Layout {
    running: bool,
    error: bool,
    hosts: Vec<(String, Vec<String>)>,
}

struct HostItems {
    submenu: Submenu,
    address: MenuItem,
    state: MenuItem,
    /// What the submenu's status dot currently shows, to skip redundant redraws.
    health: Cell<Option<Health>>,
}

struct Tray {
    icon: TrayIcon,
    icons: platform::Icons,
    /// Whether the tray currently flags a problem; `None` before the first status.
    alert: Cell<Option<bool>>,
    layout: Option<Layout>,
    header: MenuItem,
    error_item: Option<MenuItem>,
    hosts: HashMap<String, HostItems>,
    forwards: HashMap<String, CheckMenuItem>,
    report: Option<StatusReport>,
    /// The last status error, i.e. why `report` is `None`.
    status_error: Option<String>,
    /// The last failed action, shown until the next action.
    error: Option<String>,
}

impl Tray {
    fn new() -> Result<Self> {
        let header = MenuItem::new("Connecting to daemon…", false, None);
        let icons = platform::Icons::new()?;
        let builder = TrayIconBuilder::new()
            .with_tooltip("ssh-tunnel")
            .with_menu_on_left_click(true);
        let icon = icons
            .apply(builder)
            .build()
            .context("creating the tray icon")?;
        Ok(Tray {
            icon,
            icons,
            alert: Cell::new(None),
            layout: None,
            header,
            error_item: None,
            hosts: HashMap::new(),
            forwards: HashMap::new(),
            report: None,
            status_error: None,
            error: None,
        })
    }

    fn job_for(&self, action: Action) -> Option<Job> {
        Some(match action {
            Action::Toggle(name) => {
                // Clicking a check item flips its mark locally; the next
                // status puts it back to whatever the daemon decided.
                let desired = self
                    .report
                    .as_ref()?
                    .forwards
                    .iter()
                    .find(|forward| forward.name == name)?
                    .desired;
                Job::Request(Request::SetEnabled {
                    names: Selection::Names(vec![name]),
                    enabled: !desired,
                })
            }
            Action::HostEnable(host, enabled) => Job::Request(Request::SetEnabled {
                names: Selection::Names(self.forwards_of(&host)),
                enabled,
            }),
            Action::HostRestart(host) => Job::Request(Request::Restart {
                names: Selection::Names(self.forwards_of(&host)),
            }),
            Action::Reload => Job::Request(Request::Reload),
            Action::StartService => Job::StartService,
            Action::Quit => return None,
        })
    }

    fn forwards_of(&self, host: &str) -> Vec<String> {
        let Some(report) = &self.report else {
            return Vec::new();
        };
        report
            .forwards
            .iter()
            .filter(|forward| forward.host == host)
            .map(|forward| forward.name.clone())
            .collect()
    }

    fn show(&mut self, status: Result<StatusReport, String>) {
        match status {
            Ok(report) => {
                self.report = Some(report);
                self.status_error = None;
            }
            Err(message) => {
                self.report = None;
                self.status_error = Some(message);
            }
        }
        self.render();
    }

    fn show_error(&mut self, message: String) {
        self.error = Some(message);
        self.render();
    }

    fn render(&mut self) {
        let layout = self.layout();
        if self.layout.as_ref() != Some(&layout) {
            self.rebuild();
            self.layout = Some(layout);
        }
        self.update();
    }

    fn layout(&self) -> Layout {
        Layout {
            running: self.report.is_some(),
            error: self.error.is_some(),
            hosts: self
                .report
                .iter()
                .flat_map(|report| &report.hosts)
                .map(|host| (host.name.clone(), self.forwards_of(&host.name)))
                .collect(),
        }
    }

    fn rebuild(&mut self) {
        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator();
        self.hosts.clear();
        self.forwards.clear();

        self.header = MenuItem::new("", false, None);
        let mut top: Vec<Box<dyn IsMenuItem>> = vec![Box::new(self.header.clone())];
        self.error_item = self.error.as_ref().map(|_| MenuItem::new("", false, None));
        if let Some(item) = &self.error_item {
            top.push(Box::new(item.clone()));
        }
        top.push(Box::new(separator.clone()));

        if let Some(report) = &self.report {
            for host in &report.hosts {
                let address = MenuItem::new("", false, None);
                let state = MenuItem::new("", false, None);
                let submenu = Submenu::new(platform::menu_text(&host.name), true);
                let _ = submenu.append_items(&[&address, &state, &separator]);

                let forwards: Vec<&ForwardReport> = report
                    .forwards
                    .iter()
                    .filter(|forward| forward.host == host.name)
                    .collect();
                for forward in &forwards {
                    let item = CheckMenuItem::with_id(
                        Action::Toggle(forward.name.clone()).id(),
                        "",
                        true,
                        false,
                        None,
                    );
                    let _ = submenu.append(&item);
                    self.forwards.insert(forward.name.clone(), item);
                }
                if forwards.is_empty() {
                    let _ = submenu.append(&MenuItem::new("No forwards", false, None));
                }

                let name = host.name.clone();
                let has_forwards = !forwards.is_empty();
                let _ = submenu.append_items(&[
                    &separator,
                    &MenuItem::with_id(
                        Action::HostEnable(name.clone(), true).id(),
                        "Enable All",
                        has_forwards,
                        None,
                    ),
                    &MenuItem::with_id(
                        Action::HostEnable(name.clone(), false).id(),
                        "Disable All",
                        has_forwards,
                        None,
                    ),
                    &MenuItem::with_id(
                        Action::HostRestart(name.clone()).id(),
                        "Restart",
                        has_forwards,
                        None,
                    ),
                ]);

                top.push(Box::new(submenu.clone()));
                self.hosts.insert(
                    name,
                    HostItems {
                        submenu,
                        address,
                        state,
                        health: Cell::new(None),
                    },
                );
            }
            if !report.hosts.is_empty() {
                top.push(Box::new(separator.clone()));
            }
            top.push(Box::new(MenuItem::with_id(
                Action::Reload.id(),
                "Reload Config",
                true,
                None,
            )));
        } else {
            top.push(Box::new(MenuItem::with_id(
                Action::StartService.id(),
                "Start Service",
                true,
                None,
            )));
        }

        top.push(Box::new(separator.clone()));
        top.push(Box::new(MenuItem::with_id(
            Action::Quit.id(),
            platform::QUIT_LABEL,
            true,
            None,
        )));

        let refs: Vec<&dyn IsMenuItem> = top.iter().map(|item| item.as_ref()).collect();
        let _ = menu.append_items(&refs);
        self.icon.set_menu(Some(Box::new(menu)));
    }

    fn update(&self) {
        if let Some(item) = &self.error_item {
            let message = self.error.as_deref().unwrap_or_default();
            item.set_text(platform::menu_text(&format!("⚠ {}", first_line(message))));
        }

        let Some(report) = &self.report else {
            let message = self.status_error.as_deref().unwrap_or("Daemon not running");
            self.header.set_text(platform::menu_text(message));
            let _ = self
                .icon
                .set_tooltip(Some(format!("ssh-tunnel: {message}")));
            self.set_alert(true);
            return;
        };

        let wanted = report.forwards.iter().filter(|f| f.desired).count();
        let up = report
            .forwards
            .iter()
            .filter(|f| matches!(f.state, ForwardState::Listening { .. }))
            .count();
        let failing = report.forwards.iter().any(|f| f.desired && !is_up(f))
            || report
                .hosts
                .iter()
                .any(|h| matches!(h.state, HostState::Invalid { .. }));
        let summary = format!("{up} of {wanted} tunnels up");
        self.header.set_text(&summary);
        let _ = self
            .icon
            .set_tooltip(Some(format!("ssh-tunnel: {summary}")));
        self.set_alert(failing);

        for host in &report.hosts {
            let Some(items) = self.hosts.get(&host.name) else {
                continue;
            };
            let health = Health::of(&host.state);
            if items.health.replace(Some(health)) != Some(health) {
                self.icons.set_health(&items.submenu, health);
            }
            items
                .address
                .set_text(platform::menu_text(&host_address(host)));
            items
                .state
                .set_text(platform::menu_text(&host_state(&host.state)));
        }

        for forward in &report.forwards {
            let Some(item) = self.forwards.get(&forward.name) else {
                continue;
            };
            item.set_checked(forward.desired);
            item.set_text(platform::menu_text(&forward_label(forward)));
        }
    }

    fn set_alert(&self, alert: bool) {
        if self.alert.replace(Some(alert)) != Some(alert) {
            self.icons.set_alert(&self.icon, alert);
        }
    }
}

/// A host's state, reduced to the colour of its status dot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Health {
    Up,
    Pending,
    Down,
    Idle,
}

impl Health {
    fn of(state: &HostState) -> Self {
        match state {
            HostState::Connected => Health::Up,
            HostState::Connecting | HostState::Reconnecting { .. } => Health::Pending,
            HostState::Invalid { .. } => Health::Down,
            HostState::Idle => Health::Idle,
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::Result;
    use tray_icon::menu::{NativeIcon, Submenu};
    use tray_icon::{TrayIcon, TrayIconBuilder};

    use super::{Health, draw_tray_icon};

    pub const QUIT_LABEL: &str = "Quit Menu Bar Item";

    pub struct Icons;

    impl Icons {
        pub fn new() -> Result<Self> {
            Ok(Icons)
        }

        /// A black template image, which AppKit tints for light and dark menu bars.
        pub fn apply(&self, builder: TrayIconBuilder) -> TrayIconBuilder {
            match draw_tray_icon(36, [0, 0, 0], false) {
                Ok(icon) => builder.with_icon_templated(icon),
                Err(_) => builder.with_title("ssh-tunnel"),
            }
        }

        /// Template images cannot carry colour, so problems get a title badge.
        pub fn set_alert(&self, icon: &TrayIcon, alert: bool) {
            icon.set_title(alert.then_some("!"));
        }

        pub fn set_health(&self, submenu: &Submenu, health: Health) {
            submenu.set_native_icon(Some(match health {
                Health::Up => NativeIcon::StatusAvailable,
                Health::Pending => NativeIcon::StatusPartiallyAvailable,
                Health::Down => NativeIcon::StatusUnavailable,
                Health::Idle => NativeIcon::StatusNone,
            }));
        }
    }

    pub fn menu_text(text: &str) -> String {
        text.to_owned()
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::Result;
    use tray_icon::menu::{Icon as MenuIcon, Submenu};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    use super::{Health, draw_dot, draw_tray_icon};

    pub const QUIT_LABEL: &str = "Quit Tray Icon";

    /// Readable on both the light and the dark taskbar.
    const ACCENT: [u8; 3] = [0x2b, 0x7d, 0xe9];

    pub struct Icons {
        normal: Icon,
        alert: Icon,
        up: MenuIcon,
        pending: MenuIcon,
        down: MenuIcon,
        idle: MenuIcon,
    }

    impl Icons {
        pub fn new() -> Result<Self> {
            Ok(Icons {
                normal: draw_tray_icon(32, ACCENT, false)?,
                alert: draw_tray_icon(32, ACCENT, true)?,
                up: draw_dot([0x2e, 0xa0, 0x43])?,
                pending: draw_dot([0xe0, 0x9b, 0x1a])?,
                down: draw_dot([0xe5, 0x3e, 0x3e])?,
                idle: draw_dot([0x8a, 0x8a, 0x8a])?,
            })
        }

        pub fn apply(&self, builder: TrayIconBuilder) -> TrayIconBuilder {
            builder.with_icon(self.normal.clone())
        }

        /// The taskbar has no room for a title, so problems get a red badge.
        pub fn set_alert(&self, icon: &TrayIcon, alert: bool) {
            let image = if alert { &self.alert } else { &self.normal };
            let _ = icon.set_icon(Some(image.clone()));
        }

        pub fn set_health(&self, submenu: &Submenu, health: Health) {
            let dot = match health {
                Health::Up => &self.up,
                Health::Pending => &self.pending,
                Health::Down => &self.down,
                Health::Idle => &self.idle,
            };
            submenu.set_icon(Some(dot.clone()));
        }
    }

    /// Win32 menus read `&` as a mnemonic marker; `&&` is a literal ampersand.
    pub fn menu_text(text: &str) -> String {
        text.replace('&', "&&")
    }
}

fn is_up(forward: &ForwardReport) -> bool {
    matches!(forward.state, ForwardState::Listening { .. })
}

fn host_address(host: &HostReport) -> String {
    let address = match host.address.split(':').next() {
        Some(address) if address != host.alias => format!("{} → {}", host.alias, host.address),
        _ => host.address.clone(),
    };
    match &host.user {
        Some(user) => format!("{user}@{address}"),
        None => address,
    }
}

fn host_state(state: &HostState) -> String {
    match state {
        HostState::Idle => "Idle".into(),
        HostState::Connecting => "Connecting…".into(),
        HostState::Connected => "Connected".into(),
        HostState::Reconnecting { attempts, error } if *attempts > 0 => {
            format!("Reconnecting (try {attempts}): {}", first_line(error))
        }
        HostState::Reconnecting { .. } => "Reconnecting…".into(),
        HostState::Invalid { reasons } => format!("Invalid: {}", first_line(&reasons.join("; "))),
    }
}

/// `db   127.0.0.1:5432 → db.internal:5432   up · 2 conns`
fn forward_label(forward: &ForwardReport) -> String {
    let route = match (forward.kind.as_str(), &forward.target) {
        ("dynamic", _) | (_, None) => format!("SOCKS {}", forward.listen),
        ("remote", Some(target)) => format!("remote {} → {target}", forward.listen),
        (_, Some(target)) => format!("{} → {target}", forward.listen),
    };
    let state = match &forward.state {
        ForwardState::Disabled if forward.desired => "starting".into(),
        ForwardState::Failed { error } => format!("failed: {}", first_line(error)),
        ForwardState::Listening { .. } if forward.stats.active > 0 => {
            let active = forward.stats.active;
            format!("up · {active} conn{}", if active == 1 { "" } else { "s" })
        }
        other => other.label().into(),
    };
    format!("{}    {route}    {state}", forward.name)
}

/// Menu items are a single line; keep long errors from stretching the menu.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    const LIMIT: usize = 60;
    match line.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

/// A tunnel mouth over a ground line, `size` pixels square. With `badge`, a
/// red dot in the bottom-right corner, cut out from the shape behind it.
fn draw_tray_icon(size: u32, color: [u8; 3], badge: bool) -> Result<Icon> {
    const BADGE: [u8; 3] = [0xe5, 0x3e, 0x3e];
    // Designed on a 36px grid.
    let unit = size as f32 / 36.0;
    let (cx, base) = (18.0 * unit, 28.0 * unit);
    let (outer, inner) = (15.0 * unit, 8.5 * unit);
    let (badge_x, badge_y, badge_r) = (28.0 * unit, 26.0 * unit, 7.0 * unit);

    let rgba = rasterize(size, |x, y| {
        if badge {
            let distance = ((x - badge_x).powi(2) + (y - badge_y).powi(2)).sqrt();
            if distance <= badge_r {
                return Some(BADGE);
            }
            if distance <= badge_r + 2.0 * unit {
                return None;
            }
        }
        let ground =
            (base..base + 3.0 * unit).contains(&y) && (unit..size as f32 - unit).contains(&x);
        let distance = ((x - cx).powi(2) + (y - base).powi(2)).sqrt();
        let arch = y < base && (inner..=outer).contains(&distance);
        (ground || arch).then_some(color)
    });
    Icon::from_rgba(rgba, size, size).context("building the tray icon")
}

/// A status dot for menu items, which take muda's own icon type.
#[cfg_attr(not(windows), allow(dead_code))]
fn draw_dot(color: [u8; 3]) -> Result<tray_icon::menu::Icon> {
    let rgba = rasterize(16, |x, y| {
        (((x - 8.0).powi(2) + (y - 8.0).powi(2)).sqrt() <= 5.5).then_some(color)
    });
    tray_icon::menu::Icon::from_rgba(rgba, 16, 16).context("building a status icon")
}

/// Antialias a shape by supersampling: `paint` gives each sample point's colour,
/// or `None` where the image is transparent. Returns RGBA rows.
fn rasterize(size: u32, paint: impl Fn(f32, f32) -> Option<[u8; 3]>) -> Vec<u8> {
    const SAMPLES: u32 = 4;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut hits, mut sum) = (0u32, [0u32; 3]);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = px as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    if let Some(color) = paint(x, y) {
                        hits += 1;
                        for (total, channel) in sum.iter_mut().zip(color) {
                            *total += channel as u32;
                        }
                    }
                }
            }
            let average = |total: u32| total.checked_div(hits).unwrap_or(0) as u8;
            rgba.extend_from_slice(&[
                average(sum[0]),
                average(sum[1]),
                average(sum[2]),
                (hits * 255 / (SAMPLES * SAMPLES)) as u8,
            ]);
        }
    }
    rgba
}
