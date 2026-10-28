//! macOS menu bar item: one submenu per SSH host, one checkbox per forward.
//!
//! The tray is just another IPC client. A worker thread polls the daemon and
//! runs requests; the main thread owns AppKit and only ever renders reports.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tokio::sync::mpsc;
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, NativeIcon, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

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
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    // Menu bar only: no Dock icon, no app menu.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));
    let click_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(event, TrayIconEvent::Click { .. }) {
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
            // AppKit is only ready to host a status item once the loop runs.
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
                                // Give launchd a moment before the status poll.
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
}

struct Tray {
    icon: TrayIcon,
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
        let icon = TrayIconBuilder::new()
            .with_icon_templated(icon()?)
            .with_tooltip("ssh-tunnel")
            .build()
            .context("creating the menu bar item")?;
        Ok(Tray {
            icon,
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
                let submenu = Submenu::new(&host.name, true);
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
            "Quit Menu Bar Item",
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
            item.set_text(format!("⚠ {}", first_line(message)));
        }

        let Some(report) = &self.report else {
            let message = self.status_error.as_deref().unwrap_or("Daemon not running");
            self.header.set_text(message);
            let _ = self.icon.set_tooltip(Some(format!("ssh-tunnel: {message}")));
            self.icon.set_title(Some("!"));
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
        let _ = self.icon.set_tooltip(Some(format!("ssh-tunnel: {summary}")));
        self.icon.set_title(failing.then_some("!"));

        for host in &report.hosts {
            let Some(items) = self.hosts.get(&host.name) else {
                continue;
            };
            items.submenu.set_native_icon(Some(host_icon(host)));
            items.address.set_text(host_address(host));
            items.state.set_text(host_state(&host.state));
        }

        for forward in &report.forwards {
            let Some(item) = self.forwards.get(&forward.name) else {
                continue;
            };
            item.set_checked(forward.desired);
            item.set_text(forward_label(forward));
        }
    }
}

fn is_up(forward: &ForwardReport) -> bool {
    matches!(forward.state, ForwardState::Listening { .. })
}

fn host_icon(host: &HostReport) -> NativeIcon {
    match host.state {
        HostState::Connected => NativeIcon::StatusAvailable,
        HostState::Connecting | HostState::Reconnecting { .. } => {
            NativeIcon::StatusPartiallyAvailable
        }
        HostState::Invalid { .. } => NativeIcon::StatusUnavailable,
        HostState::Idle => NativeIcon::StatusNone,
    }
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

/// A tunnel mouth over a ground line, drawn as a template image so AppKit
/// tints it for light and dark menu bars.
fn icon() -> Result<Icon> {
    const SIZE: u32 = 36;
    const SAMPLES: u32 = 4;
    let (cx, base) = (SIZE as f32 / 2.0, 28.0);
    let (outer, inner) = (15.0, 8.5);

    let covered = |x: f32, y: f32| {
        let ground = (base..base + 3.0).contains(&y) && (1.0..SIZE as f32 - 1.0).contains(&x);
        let distance = ((x - cx).powi(2) + (y - base).powi(2)).sqrt();
        let arch = y < base && (inner..=outer).contains(&distance);
        ground || arch
    };

    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for py in 0..SIZE {
        for px in 0..SIZE {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = px as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    hits += covered(x, y) as u32;
                }
            }
            let alpha = (hits * 255 / (SAMPLES * SAMPLES)) as u8;
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("building the menu bar icon")
}
