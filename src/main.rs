mod popup;

use hacek::engine::{Action, Engine, Meaning, Press};
use hacek::variants::{layout_keymap, Variants};
use std::collections::VecDeque;
use std::fs::File;
use std::io::ErrorKind::WouldBlock;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard::KeymapFormat, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ContentPurpose;
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2 as grab, zwp_input_method_manager_v2 as im_manager,
    zwp_input_method_v2 as im, zwp_input_popup_surface_v2 as im_popup,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1 as vk_manager, zwp_virtual_keyboard_v1 as vk,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1 as layer_shell, zwlr_layer_surface_v1 as layer_surface,
};
use xkbcommon::xkb;

const HELP: &str = "\
hacek - hold a letter, get its accented variants

Usage: hacek [OPTIONS]

  -l, --layout LAYOUT   also take variants from an XKB layout, e.g. si or us(intl);
                        repeatable. Layouts set up in the compositor always count.
  -x, --extra X=VARS    add variants by hand, e.g. e=éèê; repeatable
  -d, --delay MS        how long to hold (default 400)
      --altgr           include letters that layouts keep behind AltGr
  -p, --position WHERE  bottom (default), center or top of the screen, or caret: under the
                        letter, or over it when there is no room (bottom if the
                        application does not say where that is). caret is EXPERIMENTAL:
                        bugs in Sway 1.12 (swaywm/sway#9343) can crash the session
      --list            print the variants from --layout and --extra, then exit
  -h, --help            show this help
  -V, --version         show the version";

/// Time for the application to handle a backspace before text is committed.
const BACKSPACE_GRACE: Duration = Duration::from_millis(20);
/// How long text waits for a text field that went away to come back.
const REFOCUS_GRACE: Duration = Duration::from_millis(100);
const KEY_BACKSPACE: u32 = 14;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;

#[derive(Clone, Copy, PartialEq)]
enum Position {
    Bottom,
    Center,
    Top,
    Caret,
}

struct Options {
    configured: Variants,
    altgr: bool,
    delay: Duration,
    position: Position,
    list: bool,
}

fn options() -> Result<Options, String> {
    let mut o = Options {
        configured: Variants::default(),
        altgr: false,
        delay: Duration::from_millis(400),
        position: Position::Bottom,
        list: false,
    };
    let (mut layouts, mut extras) = (vec![], vec![]);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "-l" | "--layout" => layouts.push(value()?),
            "-x" | "--extra" => extras.push(value()?),
            "-d" | "--delay" => {
                let ms = value()?.parse().map_err(|_| "--delay wants milliseconds")?;
                o.delay = Duration::from_millis(ms);
            }
            "--altgr" => o.altgr = true,
            "-p" | "--position" => {
                o.position = match value()?.as_str() {
                    "bottom" => Position::Bottom,
                    "center" => Position::Center,
                    "top" => Position::Top,
                    "caret" => Position::Caret,
                    other => return Err(format!("no such position: {other}")),
                }
            }
            "--list" => o.list = true,
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("hacek {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    for l in layouts {
        let keymap = layout_keymap(&l).ok_or(format!("no such keyboard layout: {l}"))?;
        o.configured.add_keymap(&keymap, o.altgr);
    }
    for x in extras {
        let (base, vars) = x.split_once('=').ok_or("--extra wants e.g. e=éè")?;
        let mut base = base.chars();
        let (Some(b), None) = (base.next(), base.next()) else {
            return Err("--extra wants a single letter before =".into());
        };
        for v in vars.chars() {
            o.configured.add(b, v);
        }
    }
    Ok(o)
}

/// Input method state; collected as pending and applied on `done`.
#[derive(Clone, Default)]
struct Field {
    active: bool,
    secret: bool,
    url: bool,
    /// Text around the cursor, with cursor and anchor byte offsets.
    surrounding: Option<(String, usize, usize)>,
}

/// Keyboard input from the grab.
enum Input {
    Key { code: u32, down: bool },
    Modifiers(u32, u32, u32, u32),
}

enum Role {
    Layer { layer: layer_surface::ZwlrLayerSurfaceV1, configured: bool },
    /// `placed` once the compositor has said where the text cursor is.
    Caret { popup: im_popup::ZwpInputPopupSurfaceV2, placed: bool },
}

/// The menu on screen. Exists only while the menu is open.
struct Window {
    surface: wl_surface::WlSurface,
    role: Role,
}

impl Drop for Window {
    fn drop(&mut self) {
        match &self.role {
            Role::Layer { layer, .. } => layer.destroy(),
            Role::Caret { popup, .. } => popup.destroy(),
        }
        self.surface.destroy();
    }
}

struct App {
    conn: Connection,
    qh: QueueHandle<App>,
    shm: wl_shm::WlShm,
    compositor: wl_compositor::WlCompositor,
    layers: layer_shell::ZwlrLayerShellV1,
    window: Option<Window>,
    im: im::ZwpInputMethodV2,
    vk: vk::ZwpVirtualKeyboardV1,
    grab: Option<grab::ZwpInputMethodKeyboardGrabV2>,
    options: Options,
    engine: Engine,
    variants: Variants,
    xkb: xkb::Context,
    keymap_text: String,
    state: Option<xkb::State>,
    mods: (u32, u32, u32, u32),
    pending: Field,
    field: Field,
    /// Whether the surrounding text was told after the last key press.
    field_is_fresh: bool,
    serial: u32,
    scale: i32,
    shown: Option<(Vec<char>, Option<usize>)>,
    /// Text to commit once the application has had time for the backspace.
    late_commit: Option<(char, Instant)>,
    /// Input held back until `late_commit` is through, so that nothing overtakes it.
    queued: VecDeque<Input>,
    started: Instant,
    fatal: Option<String>,
}

impl App {
    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn run(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Key(code, down) => self.key(code, down),
                Action::Show(variants, selected) => {
                    self.shown = Some((variants, selected));
                    if self.window.is_none() {
                        self.open(self.options.position == Position::Caret);
                    }
                    self.redraw();
                }
                Action::Hide => {
                    self.shown = None;
                    self.window = None;
                }
                Action::Replace { old, new } => self.replace(old, new),
            }
        }
    }

    fn key(&self, code: u32, down: bool) {
        self.vk.key(self.time(), code, down as u32);
    }

    fn replace(&mut self, old: char, new: char) {
        // Chrome does not report text typed with keys, so what it told before may be out of date.
        let in_text = self.field_is_fresh && self.field.surrounding.as_ref().is_some_and(|(text, cursor, anchor)| {
            cursor == anchor && text.get(..*cursor).is_some_and(|t| t.ends_with(old))
        });
        if in_text {
            self.im.delete_surrounding_text(old.len_utf8() as u32, 0);
            self.commit(new);
        } else if self.field.url {
            // Address bars complete what is typed and select the completion, so a backspace would
            // delete just that. Text replaces the selection; then remove the letter before it.
            self.commit(new);
            self.tap_plain(&[KEY_LEFT, KEY_BACKSPACE, KEY_RIGHT]);
        } else {
            // Terminals and others that do not share their text.
            self.tap_plain(&[KEY_BACKSPACE]);
            self.late_commit = Some((new, Instant::now() + BACKSPACE_GRACE));
        }
    }

    /// Tap keys without modifiers, as Shift+Backspace and friends mean other things.
    fn tap_plain(&self, codes: &[u32]) {
        let (depressed, latched, locked, group) = self.mods;
        self.vk.modifiers(0, 0, locked, group);
        for code in codes {
            self.key(*code, true);
            self.key(*code, false);
        }
        self.vk.modifiers(depressed, latched, locked, group);
    }

    fn commit(&self, c: char) {
        self.im.commit_string(c.to_string());
        self.im.commit(self.serial);
    }

    fn input(&mut self, input: Input) {
        if self.late_commit.is_some() {
            self.queued.push_back(input);
            return;
        }
        match input {
            Input::Key { code, down } => {
                let actions = if down {
                    let press = self.press(code);
                    if press.meaning != Meaning::Modifier && self.shown.is_none() {
                        self.field_is_fresh = false;
                    }
                    self.engine.press(press, Instant::now())
                } else {
                    self.engine.release(code)
                };
                self.run(actions);
            }
            Input::Modifiers(depressed, latched, locked, group) => {
                self.mods = (depressed, latched, locked, group);
                if let Some(state) = &mut self.state {
                    state.update_mask(depressed, latched, locked, 0, 0, group);
                    self.vk.modifiers(depressed, latched, locked, group);
                }
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if let Some((c, _)) = self.late_commit.filter(|(_, at)| now >= *at) {
            self.late_commit = None;
            if !self.field.active {
                self.queued.clear();
                return;
            }
            self.commit(c);
            while self.late_commit.is_none() {
                let Some(input) = self.queued.pop_front() else { break };
                self.input(input);
            }
        }
        let actions = self.engine.tick(now);
        self.run(actions);
    }

    fn deadline(&self) -> Option<Instant> {
        self.engine.deadline().into_iter().chain(self.late_commit.map(|l| l.1)).min()
    }

    fn open(&mut self, at_caret: bool) {
        let surface = self.compositor.create_surface(&self.qh, ());
        let region = self.compositor.create_region(&self.qh, ());
        surface.set_input_region(Some(&region)); // clicks go through
        region.destroy();
        let (w, h) = popup::size(self.shown.as_ref().map_or(0, |s| s.0.len()));
        let role = if at_caret {
            let popup = self.im.get_input_popup_surface(&surface, &self.qh, ());
            // Some text fields, like Chrome's address bar, do not tell where their cursor is, and
            // the menu may land off screen. So show nothing until the position is known, and
            // if it is not after this round trip, do without.
            let blank = popup::Image { width: w, height: h, stride: w * 4, data: vec![0; (w * h * 4) as usize] };
            if let Err(e) = self.attach(&surface, &blank, 1) {
                eprintln!("hacek: cannot open the menu: {e}");
            }
            self.conn.display().sync(&self.qh, surface.id());
            Role::Caret { popup, placed: false }
        } else {
            let layer = self.layers.get_layer_surface(
                &surface, None, layer_shell::Layer::Overlay, "hacek".into(), &self.qh, (),
            );
            layer.set_size(w as u32, h as u32);
            layer.set_anchor(match self.options.position {
                Position::Bottom | Position::Caret => layer_surface::Anchor::Bottom,
                Position::Top => layer_surface::Anchor::Top,
                Position::Center => layer_surface::Anchor::empty(),
            });
            layer.set_margin(100, 0, 100, 0);
            surface.commit();
            Role::Layer { layer, configured: false }
        };
        self.window = Some(Window { surface, role });
    }

    fn redraw(&self) {
        let (Some((variants, selected)), Some(window)) = (&self.shown, &self.window) else {
            return;
        };
        if matches!(
            window.role,
            Role::Layer { configured: false, .. } | Role::Caret { placed: false, .. }
        ) {
            return;
        }
        if let Err(e) = self.draw(&window.surface, variants, *selected) {
            eprintln!("hacek: cannot draw the menu: {e}");
        }
    }

    fn draw(&self, surface: &wl_surface::WlSurface, variants: &[char], selected: Option<usize>) -> Result<(), Box<dyn std::error::Error>> {
        let image = popup::draw(variants, selected, self.scale)?;
        self.attach(surface, &image, self.scale)?;
        Ok(())
    }

    fn attach(&self, surface: &wl_surface::WlSurface, image: &popup::Image, scale: i32) -> std::io::Result<()> {
        let fd = memfd(&image.data)?;
        let pool = self.shm.create_pool(fd.as_fd(), image.data.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0, image.width, image.height, image.stride, wl_shm::Format::Argb8888, &self.qh, (),
        );
        pool.destroy();
        surface.set_buffer_scale(scale);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, image.width, image.height);
        surface.commit();
        Ok(())
    }

    fn apply_field(&mut self) {
        let was = self.field.active;
        if self.field.surrounding != self.pending.surrounding {
            self.field_is_fresh = true;
        }
        self.field = self.pending.clone();
        if was && !self.field.active {
            // Some web editors (Quill) lose and regain focus when the backspace empties them.
            if let Some((_, at)) = &mut self.late_commit {
                *at = Instant::now() + REFOCUS_GRACE;
            }
            let actions = self.engine.reset();
            self.run(actions);
            let (_, _, locked, group) = self.mods;
            if self.state.is_some() {
                self.vk.modifiers(0, 0, locked, group);
            }
            if let Some(g) = self.grab.take() {
                g.release();
            }
        } else if !was && self.field.active {
            self.grab = Some(self.im.grab_keyboard(&self.qh, ()));
            if let Some((_, at)) = &mut self.late_commit {
                *at = Instant::now() + BACKSPACE_GRACE;
            }
        }
    }

    fn set_keymap(&mut self, fd: OwnedFd, size: u32) {
        let mut buf = vec![0; size as usize];
        if let Err(e) = File::from(fd).read_exact_at(&mut buf, 0) {
            return eprintln!("hacek: cannot read the keymap: {e}");
        }
        let text = String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string();
        if text == self.keymap_text {
            return;
        }
        let keymap = xkb::Keymap::new_from_string(
            &self.xkb, text.clone(), xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
        let Some(keymap) = keymap else {
            return eprintln!("hacek: the compositor sent a keymap that does not compile");
        };
        match memfd(&buf) {
            Ok(fd) => self.vk.keymap(KeymapFormat::XkbV1 as u32, fd.as_fd(), size),
            Err(e) => return eprintln!("hacek: cannot pass on the keymap: {e}"),
        }
        self.variants = self.options.configured.clone();
        self.variants.add_keymap(&keymap, self.options.altgr);
        if self.variants.is_empty() {
            eprintln!("hacek: no layout has accented letters, try e.g. --layout si");
        }
        self.state = Some(xkb::State::new(&keymap));
        self.keymap_text = text;
    }

    fn press(&self, code: u32) -> Press {
        let mut p = Press { code, letter: None, digit: None, meaning: Meaning::Other, variants: vec![] };
        let Some(state) = &self.state else { return p };
        let key = xkb::Keycode::new(code + 8);
        let sym = state.key_get_one_sym(key);
        use xkb::keysyms as k;
        p.meaning = match sym.raw() {
            k::KEY_Escape => Meaning::Escape,
            k::KEY_Left => Meaning::Left,
            k::KEY_Right => Meaning::Right,
            k::KEY_Return | k::KEY_KP_Enter | k::KEY_space => Meaning::Accept,
            _ if sym.is_modifier_key() => Meaning::Modifier,
            _ => Meaning::Other,
        };
        let shortcut = [xkb::MOD_NAME_CTRL, xkb::MOD_NAME_ALT, xkb::MOD_NAME_LOGO]
            .iter()
            .any(|m| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE));
        if shortcut {
            return p;
        }
        let keymap = state.get_keymap();
        let group = state.key_get_layout(key);
        // French types digits with shift, and shift may still be held after a capital.
        p.digit = (0..2)
            .filter(|l| *l < keymap.num_levels_for_key(key, group))
            .flat_map(|l| keymap.key_get_syms_by_level(key, group, l))
            .find_map(|s| char::from_u32(xkb::keysym_to_utf32(*s))?.to_digit(10))
            .filter(|d| *d > 0)
            .map(|d| d as usize);
        p.letter = char::from_u32(xkb::keysym_to_utf32(sym)).filter(|c| c.is_alphabetic());
        if let (Some(c), false) = (p.letter, self.field.secret) {
            p.variants = self.variants.get(c);
        }
        p
    }
}

fn memfd(data: &[u8]) -> std::io::Result<OwnedFd> {
    let fd = rustix::fs::memfd_create("hacek", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut file = File::from(fd);
    file.write_all(data)?;
    Ok(file.into())
}

impl Dispatch<im::ZwpInputMethodV2, ()> for App {
    fn event(app: &mut Self, _: &im::ZwpInputMethodV2, event: im::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            im::Event::Activate => app.pending = Field { active: true, ..Field::default() },
            im::Event::Deactivate => app.pending.active = false,
            im::Event::SurroundingText { text, cursor, anchor } => {
                app.pending.surrounding = Some((text, cursor as usize, anchor as usize));
            }
            im::Event::ContentType { purpose, .. } => {
                app.pending.secret = matches!(
                    purpose,
                    WEnum::Value(ContentPurpose::Password | ContentPurpose::Pin)
                );
                app.pending.url = matches!(purpose, WEnum::Value(ContentPurpose::Url));
            }
            im::Event::Done => {
                app.serial = app.serial.wrapping_add(1);
                app.apply_field();
            }
            im::Event::Unavailable => {
                app.fatal = Some("another input method is already running (fcitx5, ibus, hacek?)".into());
            }
            _ => {}
        }
    }
}

impl Dispatch<grab::ZwpInputMethodKeyboardGrabV2, ()> for App {
    fn event(app: &mut Self, proxy: &grab::ZwpInputMethodKeyboardGrabV2, event: grab::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if app.grab.as_ref() != Some(proxy) {
            return;
        }
        match event {
            grab::Event::Keymap { format: WEnum::Value(KeymapFormat::XkbV1), fd, size } => {
                app.set_keymap(fd, size);
            }
            grab::Event::Key { key, state, .. } => {
                let down = state == WEnum::Value(wayland_client::protocol::wl_keyboard::KeyState::Pressed);
                app.input(Input::Key { code: key, down });
            }
            grab::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                app.input(Input::Modifiers(mods_depressed, mods_latched, mods_locked, group));
            }
            _ => {}
        }
    }
}

impl Dispatch<layer_surface::ZwlrLayerSurfaceV1, ()> for App {
    fn event(app: &mut Self, proxy: &layer_surface::ZwlrLayerSurfaceV1, event: layer_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(Window { role: Role::Layer { layer, configured }, .. }) = &mut app.window else {
            return;
        };
        if layer != proxy {
            return;
        }
        match event {
            layer_surface::Event::Configure { serial, .. } => {
                layer.ack_configure(serial);
                *configured = true;
                app.redraw();
            }
            // The output went away.
            layer_surface::Event::Closed => {
                let actions = app.engine.reset();
                app.run(actions);
            }
            _ => {}
        }
    }
}

impl Dispatch<im_popup::ZwpInputPopupSurfaceV2, ()> for App {
    fn event(app: &mut Self, proxy: &im_popup::ZwpInputPopupSurfaceV2, _: im_popup::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // The only event is the position of the text cursor.
        if let Some(Window { role: Role::Caret { popup, placed }, .. }) = &mut app.window {
            if popup == proxy && !*placed {
                *placed = true;
                app.redraw();
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ObjectId> for App {
    fn event(app: &mut Self, _: &wl_callback::WlCallback, _: wl_callback::Event, id: &ObjectId, _: &Connection, _: &QueueHandle<Self>) {
        if let Some(Window { surface, role: Role::Caret { placed: false, .. } }) = &app.window {
            if surface.id() == *id {
                app.open(false);
            }
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(app: &mut Self, _: &wl_surface::WlSurface, event: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event {
            if factor != app.scale {
                app.scale = factor.max(1);
                app.redraw();
            }
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(_: &mut Self, buffer: &wl_buffer::WlBuffer, event: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_region::WlRegion);
delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore layer_shell::ZwlrLayerShellV1);
delegate_noop!(App: ignore im_manager::ZwpInputMethodManagerV2);
delegate_noop!(App: ignore vk_manager::ZwpVirtualKeyboardManagerV1);
delegate_noop!(App: ignore vk::ZwpVirtualKeyboardV1);

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = options()?;
    if options.list {
        for (base, variants) in options.configured.iter() {
            println!("{base}  {}", variants.iter().map(char::to_string).collect::<Vec<_>>().join(" "));
        }
        return Ok(());
    }

    let conn = Connection::connect_to_env().map_err(|e| format!("cannot connect to Wayland: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let unsupported = "this compositor does not let hacek work (needs input-method-v2 and \
                       virtual-keyboard-v1, as in Sway and other wlroots compositors)";
    let manager: im_manager::ZwpInputMethodManagerV2 =
        globals.bind(&qh, 1..=1, ()).map_err(|_| unsupported)?;
    let vks: vk_manager::ZwpVirtualKeyboardManagerV1 =
        globals.bind(&qh, 1..=1, ()).map_err(|_| unsupported)?;
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ())?;

    let layers: layer_shell::ZwlrLayerShellV1 =
        globals.bind(&qh, 1..=1, ()).map_err(|_| "this compositor lacks layer-shell")?;

    let im = manager.get_input_method(&seat, &qh, ());
    let vk = vks.create_virtual_keyboard(&seat, &qh, ());

    let mut app = App {
        conn: conn.clone(),
        qh,
        shm,
        compositor,
        layers,
        window: None,
        im,
        vk,
        grab: None,
        engine: Engine::new(options.delay),
        variants: Variants::default(),
        options,
        xkb: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        keymap_text: String::new(),
        state: None,
        mods: (0, 0, 0, 0),
        pending: Field::default(),
        field: Field::default(),
        field_is_fresh: false,
        serial: 0,
        scale: 1,
        shown: None,
        late_commit: None,
        queued: VecDeque::new(),
        started: Instant::now(),
        fatal: None,
    };
    loop {
        queue.dispatch_pending(&mut app)?;
        app.tick();
        if let Some(e) = app.fatal.take() {
            return Err(e.into());
        }
        let mut events = rustix::event::PollFlags::IN;
        match conn.flush() {
            Err(WaylandError::Io(e)) if e.kind() == WouldBlock => {
                events |= rustix::event::PollFlags::OUT;
            }
            result => result?,
        }
        let Some(guard) = queue.prepare_read() else { continue };
        let timeout = app.deadline().map(|at| {
            let left = at.saturating_duration_since(Instant::now());
            rustix::event::Timespec { tv_sec: left.as_secs() as _, tv_nsec: left.subsec_nanos() as _ }
        });
        let fd = guard.connection_fd();
        let mut fds = [rustix::event::PollFd::new(&fd, events)];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(0) | Err(rustix::io::Errno::INTR) => {}
            // A readable socket need not hold a complete message yet.
            Ok(_) => match guard.read() {
                Err(WaylandError::Io(e)) if e.kind() == WouldBlock => {}
                result => {
                    result?;
                }
            },
            Err(e) => return Err(e.into()),
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("hacek: {e}");
        std::process::exit(1);
    }
}
