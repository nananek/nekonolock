use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_keyboard, wl_registry, wl_seat},
    Connection, Dispatch, QueueHandle,
};

mod vk {
    // wayland_scanner が生成するコードが super::wayland_client を参照する。
    #[allow(clippy::single_component_path_imports)]
    use wayland_client;
    use wayland_client::protocol::{wl_keyboard, wl_seat};

    pub mod __interfaces {
        pub use wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/virtual-keyboard-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/virtual-keyboard-unstable-v1.xml");
}

use vk::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use vk::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn main() -> Result<()> {
    let conn = Connection::connect_to_env().context("Wayland に接続できない")?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh: QueueHandle<State> = queue.handle();

    let manager: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("zwp_virtual_keyboard_manager_v1 が無い")?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).context("wl_seat が無い")?;
    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "/tmp/wkey.xkb".to_string());
    let mut codes: Vec<u32> = args
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("キーコードを数値として解釈できない: {value}"))
        })
        .collect::<Result<_>>()?;
    if codes.is_empty() {
        // 後方互換: 引数がキーマップだけなら 1 キー（L キー）を送る。
        codes.push(38);
    }

    let keymap = std::fs::read(&path).with_context(|| format!("keymap を読めない: {path}"))?;
    let mut file = File::open(&path)?;
    file.seek(SeekFrom::Start(0))?;
    keyboard.keymap(
        wl_keyboard::KeymapFormat::XkbV1,
        file.as_fd(),
        keymap.len() as u32,
    );

    // コンポジタが virtual keyboard を seat に登録し、ロッククライアントが
    // wl_keyboard を作るのを待つ。wtype のように直後に key を送ると、
    // クライアントの wl_keyboard がまだ無く、キーが落ちる。
    conn.roundtrip()?;
    std::thread::sleep(Duration::from_millis(300));

    let start = Instant::now();

    // 最初のキーが落ちることがある（コンポジタが virtual keyboard を seat に
    // 反映してから、ロッククライアントが wl_keyboard を作るまでに隙がある）。
    // UI が無視する F13 を 1 回送って経路を温める。
    let time = start.elapsed().as_millis() as u32;
    keyboard.key(time, 183, wl_keyboard::KeyState::Pressed);
    keyboard.key(time + 10, 183, wl_keyboard::KeyState::Released);
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(150));

    for code in codes {
        let time = start.elapsed().as_millis() as u32;
        keyboard.key(time, code, wl_keyboard::KeyState::Pressed);
        keyboard.key(time + 10, code, wl_keyboard::KeyState::Released);
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(60));
    }
    conn.roundtrip()?;
    std::thread::sleep(Duration::from_millis(300));

    Ok(())
}
