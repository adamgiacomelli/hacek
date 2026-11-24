//! Fake keyboard for end-to-end tests. Reads commands from the arguments:
//! dCODE (key down), uCODE (key up), wMS (wait), mMASK (modifiers).
//! CODE is a kernel key code.

use std::{io::Write, os::fd::AsFd, time::Duration};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

struct App;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(App: ignore ZwpVirtualKeyboardV1);

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).unwrap();
    let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let vk = manager.create_virtual_keyboard(&seat, &qh, ());
    let keymap = hacek::variants::layout_keymap("us").unwrap();
    let text = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1) + "\0";
    let fd = rustix::fs::memfd_create("keymap", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(text.as_bytes()).unwrap();
    vk.keymap(1, file.as_fd(), text.len() as u32);
    for (i, arg) in std::env::args().skip(1).enumerate() {
        let n: u32 = arg[1..].parse().unwrap();
        match &arg[..1] {
            "d" => vk.key(i as u32, n, 1),
            "u" => vk.key(i as u32, n, 0),
            "m" => vk.modifiers(n, 0, 0, 0),
            "w" => std::thread::sleep(Duration::from_millis(n as u64)),
            _ => panic!("what is {arg}?"),
        }
        queue.roundtrip(&mut App).unwrap();
    }
    queue.roundtrip(&mut App).unwrap();
}
