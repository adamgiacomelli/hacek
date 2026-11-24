//! End-to-end tests: hacek and real terminals in a private headless Sway, driven by a fake
//! keyboard. Needs sway and foot, and Python with PySide6 for the Qt test:
//!
//!     cargo test --test e2e -- --ignored

use std::fs;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::sleep;
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

// Kernel key codes.
const ESC: u32 = 1;
const N1: u32 = 2;
const N2: u32 = 3;
const N9: u32 = 10;
const ENTER: u32 = 28;
const S: u32 = 31;
const D: u32 = 32;
const SHIFT: u32 = 42;
const Z: u32 = 44;
const X: u32 = 45;
const C: u32 = 46;
const RIGHT: u32 = 106;

/// hacek's default delay is 400 ms.
const LONG: u64 = 700;

struct Client;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
delegate_noop!(Client: ignore wl_seat::WlSeat);
delegate_noop!(Client: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(Client: ignore ZwpVirtualKeyboardV1);

struct Keyboard {
    queue: EventQueue<Client>,
    vk: ZwpVirtualKeyboardV1,
    time: u32,
}

impl Keyboard {
    fn new(socket: &Path) -> Self {
        let conn = Connection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
        let (globals, queue) = registry_queue_init::<Client>(&conn).unwrap();
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).unwrap();
        let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let vk = manager.create_virtual_keyboard(&seat, &qh, ());
        let keymap = hacek::variants::layout_keymap("us").unwrap();
        let text = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1) + "\0";
        let fd = rustix::fs::memfd_create("keymap", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        let mut file = fs::File::from(fd);
        file.write_all(text.as_bytes()).unwrap();
        vk.keymap(1, file.as_fd(), text.len() as u32);
        let mut keyboard = Keyboard { queue, vk, time: 0 };
        keyboard.sync();
        keyboard
    }

    fn sync(&mut self) {
        self.queue.roundtrip(&mut Client).unwrap();
    }

    fn set(&mut self, code: u32, down: bool) {
        self.time += 1;
        self.vk.key(self.time, code, down as u32);
        if code == SHIFT {
            self.vk.modifiers(down as u32, 0, 0, 0);
        }
        self.sync();
    }

    fn hold(&mut self, code: u32, ms: u64) {
        self.set(code, true);
        sleep(Duration::from_millis(ms));
        self.set(code, false);
    }

    fn tap(&mut self, code: u32) {
        self.hold(code, 20);
    }
}

struct Desktop {
    dir: PathBuf,
    sway: Child,
    hacek: Child,
    keyboard: Keyboard,
    apps: Vec<Child>,
    // Last, so that the next test starts only after this one is cleaned up.
    _turn: MutexGuard<'static, ()>,
}

impl Desktop {
    fn new() -> Self {
        Self::with(&[])
    }

    fn with(options: &[&str]) -> Self {
        // The tests depend on timing, so one at a time.
        static TURN: Mutex<()> = Mutex::new(());
        let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());

        // A runtime directory of its own keeps this apart from the user's session.
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hacek-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let mut private = fs::metadata(&dir).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut private, 0o700);
        fs::set_permissions(&dir, private).unwrap();

        let config = dir.join("config");
        fs::write(
            &config,
            "xwayland disable\n\
             output HEADLESS-1 resolution 800x400\n\
             exec echo $WAYLAND_DISPLAY $SWAYSOCK > $XDG_RUNTIME_DIR/ready\n",
        )
        .unwrap();
        let sway = Command::new("sway")
            .arg("-c")
            .arg(&config)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("SWAYSOCK")
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", "pixman")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("these tests need sway");
        let display = wait_for("sway", || {
            let ready = fs::read_to_string(dir.join("ready")).ok()?;
            ready.ends_with('\n').then(|| ready.split(' ').next().unwrap().to_string())
        });
        let keyboard = Keyboard::new(&dir.join(&display));
        let hacek = client(&dir, env!("CARGO_BIN_EXE_hacek")).args(["-l", "si"]).args(options).spawn().unwrap();
        sleep(Duration::from_millis(500));
        Desktop { dir, sway, hacek, keyboard, apps: vec![], _turn: turn }
    }

    /// Open a terminal that writes what is typed to a file, a line at a time.
    fn terminal(&mut self, name: &str) {
        let app = client(&self.dir, "foot")
            .args(["-c", "/dev/null", "sh", "-c", "exec cat > \"$1\"", "sh"])
            .arg(self.dir.join(name))
            .spawn()
            .expect("these tests need foot");
        self.apps.push(app);
        wait_for("foot", || self.dir.join(name).exists().then_some(()));
        sleep(Duration::from_millis(500));
    }

    /// Press Enter and return everything typed into the terminal so far.
    fn typed(&mut self, name: &str) -> String {
        sleep(Duration::from_millis(100));
        self.keyboard.tap(ENTER);
        sleep(Duration::from_millis(300));
        fs::read_to_string(self.dir.join(name)).unwrap()
    }

    fn swaymsg(&self, command: &str) {
        let ready = fs::read_to_string(self.dir.join("ready")).unwrap();
        let socket = ready.trim().split(' ').nth(1).unwrap();
        let ok = Command::new("swaymsg").args(["-q", "-s", socket, "--", command]).status().unwrap();
        assert!(ok.success());
    }

    fn assert_alive(&mut self) {
        assert!(self.sway.try_wait().unwrap().is_none(), "sway died");
        assert!(self.hacek.try_wait().unwrap().is_none(), "hacek died");
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        for child in self.apps.iter_mut().chain([&mut self.hacek, &mut self.sway]) {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn client(dir: &Path, program: &str) -> Command {
    let ready = fs::read_to_string(dir.join("ready")).unwrap();
    let mut command = Command::new(program);
    command
        .env_remove("DISPLAY")
        .env("WAYLAND_DISPLAY", ready.split(' ').next().unwrap())
        .env("XDG_RUNTIME_DIR", dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait_for<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(t) = ready() {
            return t;
        }
        assert!(Instant::now() < end, "{what} did not start");
        sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "needs sway and foot"]
fn picks_by_number() {
    let mut d = Desktop::new();
    d.terminal("a");
    for (letter, number) in [(C, N1), (C, N2), (S, N1), (Z, N1), (D, N1)] {
        d.keyboard.hold(letter, LONG);
        d.keyboard.tap(number);
        sleep(Duration::from_millis(50));
    }
    assert_eq!(d.typed("a"), "čćšžđ\n");
}

#[test]
#[ignore = "needs sway and foot"]
fn picks_capitals_and_with_arrows() {
    let mut d = Desktop::new();
    d.terminal("a");
    d.keyboard.set(SHIFT, true);
    d.keyboard.hold(S, LONG);
    d.keyboard.set(SHIFT, false);
    d.keyboard.tap(N1);
    sleep(Duration::from_millis(50));
    d.keyboard.hold(C, LONG);
    d.keyboard.tap(RIGHT);
    d.keyboard.tap(RIGHT);
    d.keyboard.tap(ENTER);
    assert_eq!(d.typed("a"), "Šć\n");
}

#[test]
#[ignore = "needs sway and foot"]
fn other_keys_close_the_menu() {
    let mut d = Desktop::new();
    d.terminal("a");
    d.keyboard.hold(C, LONG);
    d.keyboard.tap(ESC);
    d.keyboard.tap(N1);
    d.keyboard.hold(C, LONG);
    d.keyboard.tap(N9);
    d.keyboard.hold(C, LONG);
    d.keyboard.tap(X);
    assert_eq!(d.typed("a"), "c1c9cx\n");
}

#[test]
#[ignore = "needs sway and foot"]
fn taps_do_not_open_and_other_letters_repeat() {
    let mut d = Desktop::new();
    d.terminal("a");
    d.keyboard.tap(C);
    d.keyboard.tap(N1);
    d.keyboard.hold(X, 1000);
    let typed = d.typed("a");
    let xs = typed.trim_end().strip_prefix("c1").expect(&typed);
    assert!(xs.len() > 2 && xs.chars().all(|c| c == 'x'), "{typed:?}");
}

#[test]
#[ignore = "needs sway and foot"]
fn typing_on_at_once_keeps_order() {
    let mut d = Desktop::new();
    d.terminal("a");
    for _ in 0..3 {
        d.keyboard.hold(D, LONG);
        for code in [N1, X] {
            d.keyboard.set(code, true);
            d.keyboard.set(code, false);
        }
        sleep(Duration::from_millis(100));
    }
    assert_eq!(d.typed("a"), "đxđxđx\n");
}

/// Qt handles keys later than committed text. Keys right after a pick, even the release of the
/// number, once made hacek commit too early, and the backspace ate the accent. Terminals do not
/// show this.
#[test]
#[ignore = "needs sway and PySide6"]
fn typing_on_at_once_keeps_order_in_qt() {
    const APP: &str = "
import sys
from PySide6.QtWidgets import QApplication, QLineEdit
app = QApplication([])
edit = QLineEdit()
def save(text):
    with open(sys.argv[1], 'w') as f:
        f.write(text)
edit.textChanged.connect(save)
edit.show()
edit.setFocus()
app.exec()
";
    let mut d = Desktop::new();
    let file = d.dir.join("qt");
    let mut app = client(&d.dir, "python3")
        .args(["-c", APP])
        .arg(&file)
        .env("QT_QPA_PLATFORM", "wayland")
        .env_remove("QT_IM_MODULE")
        .env_remove("QT_IM_MODULES")
        .spawn()
        .expect("this test needs python3");
    sleep(Duration::from_secs(3));
    assert!(app.try_wait().unwrap().is_none(), "the Qt application died; is PySide6 installed?");
    d.apps.push(app);
    for _ in 0..3 {
        d.keyboard.hold(D, LONG);
        for code in [N1, X] {
            d.keyboard.set(code, true);
            d.keyboard.set(code, false);
        }
        sleep(Duration::from_millis(100));
    }
    sleep(Duration::from_millis(300));
    assert_eq!(fs::read_to_string(&file).unwrap(), "đxđxđx");
}

#[test]
#[ignore = "needs sway and foot"]
fn focus_change_closes_the_menu() {
    let mut d = Desktop::new();
    d.terminal("a");
    d.terminal("b");
    d.keyboard.hold(C, LONG);
    d.swaymsg("focus left");
    sleep(Duration::from_millis(200));
    d.keyboard.tap(N1);
    assert_eq!(d.typed("a"), "1\n");
    d.swaymsg("focus right");
    sleep(Duration::from_millis(200));
    d.keyboard.tap(X);
    assert_eq!(d.typed("b"), "cx\n");
    d.assert_alive();
}

#[test]
#[ignore = "needs sway and foot"]
fn survives_a_window_closing_under_the_menu() {
    let mut d = Desktop::new();
    d.terminal("a");
    d.keyboard.hold(C, LONG);
    let mut app = d.apps.pop().unwrap();
    app.kill().unwrap();
    app.wait().unwrap();
    sleep(Duration::from_millis(200));
    d.terminal("b");
    d.keyboard.hold(C, LONG);
    d.keyboard.tap(N1);
    assert_eq!(d.typed("b"), "č\n");
    d.assert_alive();
}

#[test]
#[ignore = "needs sway and foot"]
fn keyboard_works_after_hacek_is_killed() {
    let mut d = Desktop::new();
    d.terminal("a");
    d.keyboard.hold(C, LONG);
    d.hacek.kill().unwrap();
    d.hacek.wait().unwrap();
    sleep(Duration::from_millis(200));
    d.keyboard.tap(N1);
    d.keyboard.tap(X);
    assert_eq!(d.typed("a"), "c1x\n");
}

/// Focus changes and the like just as the menu opens. hacek once took a partly received
/// Wayland message for a lost connection and quit.
#[test]
#[ignore = "needs sway and foot"]
fn survives_upheaval() {
    let commands = [
        "focus left",
        "focus right",
        "output HEADLESS-1 scale 2",
        "output HEADLESS-1 scale 1",
        "output HEADLESS-1 power off",
        "output HEADLESS-1 power on",
        "workspace 2",
        "workspace 1",
        "input * xkb_layout 'us,si'",
        "input * xkb_layout us",
    ];
    let mut d = Desktop::new();
    d.terminal("a");
    d.terminal("b");
    for (i, command) in commands.iter().cycle().take(30).enumerate() {
        d.keyboard.hold([C, S, D, Z][i % 4], if i % 5 == 0 { 405 } else { 430 });
        d.swaymsg(command);
        d.keyboard.tap([N1, N9, ESC, RIGHT][i % 4]);
        d.assert_alive();
    }
}

#[test]
#[ignore = "needs sway and foot"]
fn every_position_works() {
    for position in ["top", "center", "caret"] {
        let mut d = Desktop::with(&["--position", position]);
        d.terminal("a");
        d.keyboard.hold(C, LONG);
        d.keyboard.tap(N1);
        assert_eq!(d.typed("a"), "č\n", "{position}");
        d.assert_alive();
    }
}

#[test]
#[ignore = "needs sway and foot"]
fn second_instance_refuses() {
    let d = Desktop::new();
    let out = client(&d.dir, env!("CARGO_BIN_EXE_hacek")).stderr(Stdio::piped()).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
}
