//! A trawld that is killed and started again on the same session bus comes
//! back with the resources the previous one held.
//!
//! Every test runs the trawld binary on a private session bus of its own,
//! with a private XDG_RUNTIME_DIR, so the user's session is left alone.

use std::{
    collections::HashMap,
    fs::{self, Permissions},
    io::{BufRead, BufReader, ErrorKind},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;
use zbus::{connection, fdo::DBusProxy, names::BusName, proxy, proxy::CacheProperties, Connection};

const NAME: &str = "org.regolith.Trawl";
const TIMEOUT: Duration = Duration::from_secs(10);

#[proxy(
    default_service = "org.regolith.Trawl",
    default_path = "/org/regolith/Trawl",
    interface = "org.regolith.trawl1"
)]
trait Trawl {
    fn set_resource(&self, key: &str, val: &str) -> zbus::Result<()>;
    fn add_resource(&self, key: &str, val: &str) -> zbus::Result<()>;
    fn remove_one(&self, key: &str) -> zbus::Result<()>;
    fn merge_text(&self, text: &str) -> zbus::Result<()>;
    fn get_resource(&self, key: &str) -> zbus::Result<String>;
    #[zbus(property)]
    fn resources(&self) -> zbus::Result<HashMap<String, String>>;
}

/// A child process, killed when dropped
struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Process {
    /// Waits for the process to exit by itself
    async fn exit_status(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the process did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// A scratch directory holding the runtime directory, removed when dropped
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        let dir = std::env::temp_dir().join(format!("trawld-restart-{}", Uuid::new_v4()));
        for dir in [&dir, &dir.join("run")] {
            fs::create_dir(dir).unwrap();
            fs::set_permissions(dir, Permissions::from_mode(0o700)).unwrap();
        }
        Scratch(dir)
    }

    fn runtime_dir(&self) -> PathBuf {
        self.0.join("run")
    }

    fn snapshot_dir(&self) -> PathBuf {
        self.runtime_dir().join("trawl")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A private session bus
struct Bus {
    address: String,
    _daemon: Process,
}

impl Bus {
    /// Returns None if dbus-daemon isn't installed
    fn start(dir: &Path) -> Option<Bus> {
        let listen = format!("--address=unix:tmpdir={}", dir.display());
        let spawned = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address", &listen])
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                eprintln!("dbus-daemon not found, skipping");
                return None;
            }
            Err(e) => panic!("failed to start dbus-daemon: {e}"),
        };
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Some(Bus {
            address: address.trim().to_owned(),
            _daemon: Process(child),
        })
    }

    async fn connect(&self) -> Connection {
        connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    async fn name_has_owner(&self, conn: &Connection) -> bool {
        DBusProxy::new(conn)
            .await
            .unwrap()
            .name_has_owner(BusName::try_from(NAME).unwrap())
            .await
            .unwrap()
    }

    /// Starts trawld with `args`, without waiting for it
    fn spawn_trawld(&self, scratch: &Scratch, args: &[&str]) -> Process {
        let child = Command::new(env!("CARGO_BIN_EXE_trawld"))
            .args(args)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .env("XDG_RUNTIME_DIR", scratch.runtime_dir())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        Process(child)
    }

    /// Starts trawld with `args` and waits until it owns its name
    async fn start_trawld(&self, scratch: &Scratch, args: &[&str]) -> Process {
        let mut trawld = self.spawn_trawld(scratch, args);
        let conn = self.connect().await;
        let deadline = Instant::now() + TIMEOUT;
        while !self.name_has_owner(&conn).await {
            if let Some(status) = trawld.0.try_wait().unwrap() {
                panic!("trawld exited with {status}");
            }
            assert!(Instant::now() < deadline, "trawld did not take its name");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        trawld
    }

    /// Kills trawld without warning and waits until its name is free
    async fn kill_trawld(&self, trawld: Process) {
        drop(trawld);
        let conn = self.connect().await;
        let deadline = Instant::now() + TIMEOUT;
        while self.name_has_owner(&conn).await {
            assert!(Instant::now() < deadline, "the name stayed taken");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn proxy(&self) -> TrawlProxy<'static> {
        TrawlProxy::builder(&self.connect().await)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn resources_survive_a_kill() {
    let scratch = Scratch::new();
    let Some(bus) = Bus::start(&scratch.0) else {
        return;
    };
    let trawld = bus.start_trawld(&scratch, &[]).await;
    let trawl = bus.proxy().await;
    let tricky = "a\tb: c\n!d #e \\f \\U000000e9 café \u{FFFD}";
    trawl.merge_text("wm.gaps: 10\ngone: soon\n").await.unwrap();
    trawl.set_resource("tricky", tricky).await.unwrap();
    trawl.set_resource("empty", "").await.unwrap();
    trawl.remove_one("gone").await.unwrap();
    let before = trawl.resources().await.unwrap();
    assert_eq!(before.len(), 3);

    bus.kill_trawld(trawld).await;
    let _trawld = bus.start_trawld(&scratch, &[]).await;
    let trawl = bus.proxy().await;
    assert_eq!(trawl.resources().await.unwrap(), before);
    assert_eq!(trawl.get_resource("tricky").await.unwrap(), tricky);
    // A restored resource is defined: adding doesn't replace it
    trawl.add_resource("wm.gaps", "99").await.unwrap();
    assert_eq!(trawl.get_resource("wm.gaps").await.unwrap(), "10");
    let mode = fs::metadata(scratch.snapshot_dir())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[tokio::test]
async fn startup_file_only_adds_what_is_missing() {
    let scratch = Scratch::new();
    let Some(bus) = Bus::start(&scratch.0) else {
        return;
    };
    let trawld = bus.start_trawld(&scratch, &[]).await;
    bus.proxy().await.set_resource("a", "saved").await.unwrap();
    bus.kill_trawld(trawld).await;

    let file = scratch.0.join("look");
    fs::write(&file, "a: file\nb: file\n").unwrap();
    let file = file.to_str().unwrap();
    let _trawld = bus.start_trawld(&scratch, &["--nocpp", file]).await;
    let expected: HashMap<String, String> = [("a", "saved"), ("b", "file")]
        .iter()
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(bus.proxy().await.resources().await.unwrap(), expected);
}

#[tokio::test]
async fn another_bus_starts_empty() {
    let scratch = Scratch::new();
    let Some(first) = Bus::start(&scratch.0) else {
        return;
    };
    let trawld = first.start_trawld(&scratch, &[]).await;
    first.proxy().await.set_resource("a", "1").await.unwrap();
    first.kill_trawld(trawld).await;

    let second = Bus::start(&scratch.0).unwrap();
    let trawld = second.start_trawld(&scratch, &[]).await;
    assert!(second.proxy().await.resources().await.unwrap().is_empty());
    second.kill_trawld(trawld).await;

    // The snapshot of the first bus was replaced, so it doesn't come back
    let _trawld = first.start_trawld(&scratch, &[]).await;
    assert!(first.proxy().await.resources().await.unwrap().is_empty());
}

#[tokio::test]
async fn second_instance_leaves_the_snapshot_alone() {
    let scratch = Scratch::new();
    let Some(bus) = Bus::start(&scratch.0) else {
        return;
    };
    let trawld = bus.start_trawld(&scratch, &[]).await;
    bus.proxy().await.set_resource("a", "1").await.unwrap();
    let saved = fs::read(scratch.snapshot_dir().join("resources")).unwrap();

    let file = scratch.0.join("look");
    fs::write(&file, "b: 2\n").unwrap();
    let mut second = bus.spawn_trawld(&scratch, &["--nocpp", file.to_str().unwrap()]);
    assert!(!second.exit_status().await.success());
    assert_eq!(
        fs::read(scratch.snapshot_dir().join("resources")).unwrap(),
        saved
    );

    bus.kill_trawld(trawld).await;
    let _trawld = bus.start_trawld(&scratch, &[]).await;
    let resources = bus.proxy().await.resources().await.unwrap();
    assert_eq!(
        resources,
        HashMap::from([("a".to_string(), "1".to_string())])
    );
}

#[tokio::test]
async fn unsafe_snapshot_dir_is_not_used() {
    let scratch = Scratch::new();
    let Some(bus) = Bus::start(&scratch.0) else {
        return;
    };
    fs::create_dir(scratch.snapshot_dir()).unwrap();
    fs::set_permissions(scratch.snapshot_dir(), Permissions::from_mode(0o777)).unwrap();
    let trawld = bus.start_trawld(&scratch, &[]).await;
    bus.proxy().await.set_resource("a", "1").await.unwrap();
    assert_eq!(fs::read_dir(scratch.snapshot_dir()).unwrap().count(), 0);

    bus.kill_trawld(trawld).await;
    let _trawld = bus.start_trawld(&scratch, &[]).await;
    assert!(bus.proxy().await.resources().await.unwrap().is_empty());
}
