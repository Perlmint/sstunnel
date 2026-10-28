//! End-to-end tests against an in-process SSH server.

mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use ssh_tunnel::cli::DaemonArgs;
use ssh_tunnel::config::Config;
use ssh_tunnel::ipc::client;
use ssh_tunnel::ipc::protocol::{
    ForwardReport, ForwardState, HostReport, HostState, Request, Response, Selection, StatusReport,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use support::{Breaker, TestSshServer, echo_server, wait_for};

/// A daemon running in this process, driven over its real control socket.
struct Agent {
    dir: TempDir,
    config_path: PathBuf,
    endpoint: String,
    task: JoinHandle<()>,
}

impl Agent {
    async fn start(dir: TempDir, config_text: &str) -> Self {
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, config_text).unwrap();

        let endpoint = unique_endpoint();
        let config = Config::load(&config_path).expect("the test config should be valid");
        let args = DaemonArgs {
            log_level: None,
            log_file: None,
            enable: vec![],
            disable: vec![],
            no_watch: false,
            no_persist_state: true,
        };

        let task = {
            let config_path = config_path.clone();
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                ssh_tunnel::daemon::run(config_path, endpoint, config, &args)
                    .await
                    .expect("the daemon should run");
            })
        };

        let agent = Self {
            dir,
            config_path,
            endpoint,
            task,
        };
        wait_for("the control socket to answer", || async {
            client::probe(&agent.endpoint).await
        })
        .await;
        agent
    }

    async fn request(&self, request: Request) -> Response {
        client::request(&self.endpoint, &request).await.unwrap()
    }

    async fn status(&self) -> StatusReport {
        match self.request(Request::Status).await {
            Response::Status(report) => *report,
            other => panic!("expected a status report, got {other:?}"),
        }
    }

    async fn try_forward(&self, name: &str) -> Option<ForwardReport> {
        self.status()
            .await
            .forwards
            .into_iter()
            .find(|forward| forward.name == name)
    }

    async fn host(&self, name: &str) -> HostReport {
        self.status()
            .await
            .hosts
            .into_iter()
            .find(|host| host.name == name)
            .unwrap_or_else(|| panic!("no host named `{name}`"))
    }

    async fn forward(&self, name: &str) -> ForwardReport {
        self.try_forward(name)
            .await
            .unwrap_or_else(|| panic!("no forward named `{name}`"))
    }

    /// Wait until the named forward reports `Listening`, and return its address.
    /// The forward may not exist yet, which is the case right after a reload.
    async fn wait_listening(&self, name: &str) -> String {
        wait_for(&format!("forward `{name}` to come up"), || async {
            matches!(
                self.try_forward(name).await.map(|forward| forward.state),
                Some(ForwardState::Listening { .. })
            )
        })
        .await;
        match self.forward(name).await.state {
            ForwardState::Listening { addr } => addr,
            other => panic!("expected `{name}` to be listening, got {other:?}"),
        }
    }

    fn rewrite_config(&self, config_text: &str) {
        // Write-then-rename, the way an editor saves, which is exactly the case
        // a naive watch on the file inode would miss.
        let temp = self.dir.path().join("config.toml.new");
        std::fs::write(&temp, config_text).unwrap();
        std::fs::rename(&temp, &self.config_path).unwrap();
    }

    async fn shutdown(self) {
        let _ = self.request(Request::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

fn unique_endpoint() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    #[cfg(windows)]
    {
        format!("ssh-tunnel-test-{}-{id}.sock", std::process::id())
    }
    #[cfg(unix)]
    {
        // Kept short: unix socket paths are capped at ~104 bytes.
        format!("/tmp/sshtun-t{}-{id}.sock", std::process::id())
    }
}

/// A config with one host pointing at `ssh_addr` and the given forward table.
fn config_for(
    dir: &TempDir,
    server: &TestSshServer,
    ssh_addr: SocketAddr,
    forwards: &str,
) -> String {
    let key = server.write_client_key(dir.path());
    let known_hosts = dir.path().join("known_hosts");
    format!(
        r#"
[hosts.test]
hostname = "{host}"
port = {port}
user = "tester"
identity = ["{key}"]
use_agent = false
known_hosts = "{known_hosts}"
strict_host_key_checking = "accept-new"
connect_timeout = "5s"
keepalive_interval = "5s"
reconnect = {{ initial = "200ms", max = "1s", factor = 2.0 }}
{forwards}
"#,
        host = ssh_addr.ip(),
        port = ssh_addr.port(),
        key = key.display(),
        known_hosts = known_hosts.display(),
    )
}

async fn round_trip(addr: &str, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(payload).await?;
    let mut buffer = vec![0u8; payload.len()];
    stream.read_exact(&mut buffer).await?;
    Ok(buffer)
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_forward_carries_traffic_and_can_be_toggled() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let config = config_for(
        &dir,
        &server,
        server.addr,
        &format!(
            r#"
[[forwards]]
name = "echo"
host = "test"
type = "local"
listen = "127.0.0.1:0"
target = "{echo}"
"#
        ),
    );
    let agent = Agent::start(dir, &config).await;
    let addr = agent.wait_listening("echo").await;

    assert_eq!(round_trip(&addr, b"hello").await.unwrap(), b"hello");

    let stats = agent.forward("echo").await.stats;
    assert_eq!(stats.total, 1, "the connection should be counted");
    assert_eq!(stats.bytes_out, 5);
    assert_eq!(stats.bytes_in, 5);

    // Turning it off closes the listener.
    agent
        .request(Request::SetEnabled {
            names: Selection::Names(vec!["echo".into()]),
            enabled: false,
        })
        .await;
    wait_for("the listener to close", || async {
        round_trip(&addr, b"x").await.is_err()
    })
    .await;
    assert!(!agent.forward("echo").await.desired);

    // Turning it back on rebinds the same port.
    agent
        .request(Request::SetEnabled {
            names: Selection::Names(vec!["echo".into()]),
            enabled: true,
        })
        .await;
    let rebound = agent.wait_listening("echo").await;
    assert_eq!(round_trip(&rebound, b"again").await.unwrap(), b"again");

    agent.shutdown().await;
}

#[tokio::test]
async fn dynamic_forward_serves_socks5_connect() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let config = config_for(
        &dir,
        &server,
        server.addr,
        r#"
[[forwards]]
name = "socks"
host = "test"
type = "dynamic"
listen = "127.0.0.1:0"
"#,
    );
    let agent = Agent::start(dir, &config).await;
    let addr = agent.wait_listening("socks").await;

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    // Greeting: version 5, one method, "no authentication".
    stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut greeting = [0u8; 2];
    stream.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting, [0x05, 0x00]);

    // CONNECT to the echo server by IPv4 address.
    let SocketAddr::V4(echo_v4) = echo else {
        panic!("expected an IPv4 echo server")
    };
    let mut request = vec![0x05, 0x01, 0x00, 0x01];
    request.extend_from_slice(&echo_v4.ip().octets());
    request.extend_from_slice(&echo_v4.port().to_be_bytes());
    stream.write_all(&request).await.unwrap();

    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[0..2], [0x05, 0x00], "the proxy should report success");

    stream.write_all(b"socks").await.unwrap();
    let mut echoed = [0u8; 5];
    stream.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"socks");

    agent.shutdown().await;
}

#[tokio::test]
async fn remote_forward_delivers_inbound_connections() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let config = config_for(
        &dir,
        &server,
        server.addr,
        &format!(
            r#"
[[forwards]]
name = "back"
host = "test"
type = "remote"
listen = "127.0.0.1:0"
target = "{echo}"
"#
        ),
    );
    let agent = Agent::start(dir, &config).await;
    let addr = agent.wait_listening("back").await;

    assert_eq!(round_trip(&addr, b"inbound").await.unwrap(), b"inbound");
    assert_eq!(agent.forward("back").await.stats.total, 1);

    agent.shutdown().await;
}

#[tokio::test]
async fn forwards_recover_after_the_link_drops() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let breaker = Breaker::start(server.addr).await;
    let dir = tempfile::tempdir().unwrap();

    let config = config_for(
        &dir,
        &server,
        breaker.addr,
        &format!(
            r#"
[[forwards]]
name = "echo"
host = "test"
type = "local"
listen = "127.0.0.1:0"
target = "{echo}"

[[forwards]]
name = "back"
host = "test"
type = "remote"
listen = "127.0.0.1:0"
target = "{echo}"
"#
        ),
    );
    let agent = Agent::start(dir, &config).await;
    let local_addr = agent.wait_listening("echo").await;
    let remote_addr = agent.wait_listening("back").await;
    assert_eq!(round_trip(&local_addr, b"before").await.unwrap(), b"before");
    assert_eq!(
        round_trip(&remote_addr, b"before").await.unwrap(),
        b"before"
    );

    breaker.break_all();

    // The local listener must stay bound through the outage, so the port is
    // never lost to another process.
    wait_for("the forward to notice the outage", || async {
        agent.forward("echo").await.state == ForwardState::WaitingForHost
    })
    .await;
    assert!(
        TcpStream::connect(&local_addr).await.is_ok(),
        "the local listener should stay bound while the host is down",
    );

    let recovered = agent.wait_listening("echo").await;
    assert_eq!(recovered, local_addr, "the local forward keeps its port");
    assert_eq!(round_trip(&local_addr, b"after").await.unwrap(), b"after");

    // The remote forward has to be re-requested on the new session.
    let remote_again = agent.wait_listening("back").await;
    assert_eq!(round_trip(&remote_again, b"after").await.unwrap(), b"after");

    agent.shutdown().await;
}

#[tokio::test]
async fn config_changes_are_picked_up_and_bad_ones_are_ignored() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let one = format!(
        r#"
[[forwards]]
name = "echo"
host = "test"
type = "local"
listen = "127.0.0.1:0"
target = "{echo}"
"#
    );
    let config = config_for(&dir, &server, server.addr, &one);
    let agent = Agent::start(dir, &config).await;
    let addr = agent.wait_listening("echo").await;

    // Adding a forward brings it up without touching the existing one.
    agent.rewrite_config(&format!(
        "{config}\n[[forwards]]\nname = \"second\"\nhost = \"test\"\ntype = \"local\"\n\
         listen = \"127.0.0.1:0\"\ntarget = \"{echo}\"\n"
    ));
    let second = agent.wait_listening("second").await;
    assert_eq!(round_trip(&second, b"two").await.unwrap(), b"two");
    assert_eq!(
        agent.wait_listening("echo").await,
        addr,
        "an untouched forward must not be restarted",
    );

    // A broken config must leave the running tunnels alone.
    agent.rewrite_config("this is not valid toml =");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(round_trip(&addr, b"still").await.unwrap(), b"still");
    assert_eq!(agent.status().await.forwards.len(), 2);

    agent.shutdown().await;
}

#[tokio::test]
async fn unknown_forward_names_are_rejected() {
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(&dir, &server, server.addr, "");
    let agent = Agent::start(dir, &config).await;

    let response = agent
        .request(Request::SetEnabled {
            names: Selection::Names(vec!["nope".into()]),
            enabled: true,
        })
        .await;
    match response {
        Response::Error { message } => assert!(message.contains("nope"), "{message}"),
        other => panic!("expected an error, got {other:?}"),
    }

    agent.shutdown().await;
}

// ---------------------------------------------------------------------------
// ~/.ssh/config resolution
// ---------------------------------------------------------------------------

/// Write an ssh config describing the test server under `alias`, and return a
/// TOML config whose host body is `host_body` and which points at that file.
fn config_via_ssh_config(
    dir: &TempDir,
    server: &TestSshServer,
    ssh_addr: SocketAddr,
    alias: &str,
    directives: &str,
    hosts: &str,
    forwards: &str,
) -> String {
    let key = server.write_client_key(dir.path());
    let ssh_config_path = dir.path().join("ssh_config");
    std::fs::write(
        &ssh_config_path,
        format!(
            "Host {alias}\n  HostName {ip}\n  Port {port}\n  IdentityFile {key}\n{directives}",
            ip = ssh_addr.ip(),
            port = ssh_addr.port(),
            key = key.display(),
        ),
    )
    .unwrap();

    format!(
        r#"
[daemon]
ssh_config = "{ssh_config}"
{hosts}
{forwards}
"#,
        ssh_config = ssh_config_path.display(),
    )
}

/// The passthrough settings every test host needs, whatever else it resolves.
fn host_defaults(dir: &TempDir) -> String {
    format!(
        r#"use_agent = false
known_hosts = "{known_hosts}"
strict_host_key_checking = "accept-new"
connect_timeout = "5s"
keepalive_interval = "5s"
reconnect = {{ initial = "200ms", max = "1s", factor = 2.0 }}"#,
        known_hosts = dir.path().join("known_hosts").display(),
    )
}

#[tokio::test]
async fn a_host_resolves_entirely_from_ssh_config() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    // The TOML says only which alias to use; hostname, port, user, and the
    // identity all have to come out of the ssh config.
    let config = config_via_ssh_config(
        &dir,
        &server,
        server.addr,
        "testalias",
        "  User tester\n",
        &format!(
            "[hosts.test]\nhostname = \"testalias\"\n{}",
            host_defaults(&dir)
        ),
        &format!(
            r#"
[[forwards]]
name = "echo"
host = "test"
type = "local"
listen = "127.0.0.1:0"
target = "{echo}"
"#
        ),
    );
    let agent = Agent::start(dir, &config).await;
    let addr = agent.wait_listening("echo").await;

    assert_eq!(round_trip(&addr, b"resolved").await.unwrap(), b"resolved");

    let host = agent.host("test").await;
    assert_eq!(host.alias, "testalias");
    assert_eq!(host.user.as_deref(), Some("tester"));
    assert_eq!(
        host.address,
        format!("{}:{}", server.addr.ip(), server.addr.port())
    );
    let sources = host.sources.expect("a valid host reports its sources");
    assert_eq!(sources.user, ssh_tunnel::config::ValueSource::SshConfig);
    assert_eq!(sources.address, ssh_tunnel::config::ValueSource::SshConfig);
    assert_eq!(sources.identity, ssh_tunnel::config::ValueSource::SshConfig);

    agent.shutdown().await;
}

#[tokio::test]
async fn the_agent_config_overrides_ssh_config() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    // The ssh config names the wrong user; ours must win.
    let config = config_via_ssh_config(
        &dir,
        &server,
        server.addr,
        "testalias",
        "  User wrong-user\n",
        &format!(
            "[hosts.test]\nhostname = \"testalias\"\nuser = \"tester\"\n{}",
            host_defaults(&dir)
        ),
        &format!(
            r#"
[[forwards]]
name = "echo"
host = "test"
type = "local"
listen = "127.0.0.1:0"
target = "{echo}"
"#
        ),
    );
    let agent = Agent::start(dir, &config).await;
    let addr = agent.wait_listening("echo").await;
    assert_eq!(round_trip(&addr, b"ours").await.unwrap(), b"ours");

    let host = agent.host("test").await;
    assert_eq!(host.user.as_deref(), Some("tester"));
    assert_eq!(
        host.sources.unwrap().user,
        ssh_tunnel::config::ValueSource::Config,
    );

    agent.shutdown().await;
}

#[tokio::test]
async fn an_unresolvable_host_is_marked_invalid_without_taking_others_down() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    // `broken` names an alias the ssh config knows nothing about, and supplies
    // no user of its own.
    let hosts = format!(
        "[hosts.test]\nhostname = \"testalias\"\n{defaults}\n\n         [hosts.broken]\nhostname = \"nowhere\"\n{defaults}",
        defaults = host_defaults(&dir),
    );
    let config = config_via_ssh_config(
        &dir,
        &server,
        server.addr,
        "testalias",
        "  User tester\n",
        &hosts,
        &format!(
            r#"
[[forwards]]
name = "good"
host = "test"
type = "local"
listen = "127.0.0.1:0"
target = "{echo}"

[[forwards]]
name = "bad"
host = "broken"
type = "local"
listen = "127.0.0.1:0"
target = "{echo}"
"#
        ),
    );
    let agent = Agent::start(dir, &config).await;

    // The healthy host is entirely unaffected.
    let good = agent.wait_listening("good").await;
    assert_eq!(round_trip(&good, b"fine").await.unwrap(), b"fine");

    let broken = agent.host("broken").await;
    match &broken.state {
        HostState::Invalid { reasons } => {
            assert!(
                reasons.iter().any(|r| r.contains("no username")),
                "{reasons:?}"
            );
            assert!(reasons.iter().any(|r| r.contains("nowhere")), "{reasons:?}");
        }
        other => panic!("expected `broken` to be invalid, got {other:?}"),
    }
    assert_eq!(broken.user, None);
    assert!(broken.sources.is_none());

    // Its forward is reported as failed with the reason, not silently "off".
    let bad = agent.forward("bad").await;
    assert!(bad.desired, "it is still meant to be running");
    match &bad.state {
        ForwardState::Failed { error } => {
            assert!(error.contains("unusable"), "{error}");
        }
        other => panic!("expected `bad` to have failed, got {other:?}"),
    }

    agent.shutdown().await;
}

#[tokio::test]
async fn filling_in_the_missing_value_and_reloading_brings_the_host_up() {
    let echo = echo_server().await;
    let server = TestSshServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let with_user = |user: &str| {
        format!(
            "[hosts.test]\nhostname = \"testalias\"\n{user}{defaults}",
            defaults = host_defaults(&dir),
        )
    };
    let forwards = format!(
        r#"
[[forwards]]
name = "echo"
host = "test"
type = "local"
listen = "127.0.0.1:0"
target = "{echo}"
"#
    );
    // The ssh config resolves everything except the user.
    let broken = config_via_ssh_config(
        &dir,
        &server,
        server.addr,
        "testalias",
        "",
        &with_user(""),
        &forwards,
    );
    let fixed = config_via_ssh_config(
        &dir,
        &server,
        server.addr,
        "testalias",
        "",
        &with_user("user = \"tester\"\n"),
        &forwards,
    );

    let agent = Agent::start(dir, &broken).await;
    assert!(matches!(
        agent.host("test").await.state,
        HostState::Invalid { .. }
    ));

    agent.rewrite_config(&fixed);
    agent.request(Request::Reload).await;

    let addr = agent.wait_listening("echo").await;
    assert_eq!(round_trip(&addr, b"fixed").await.unwrap(), b"fixed");
    assert!(
        !agent
            .status()
            .await
            .forwards
            .iter()
            .any(|f| matches!(f.state, ForwardState::Failed { .. })),
        "nothing should still be failing",
    );

    agent.shutdown().await;
}
