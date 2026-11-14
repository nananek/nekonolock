//! `nekonolock`: Sway 用スクリーンロッカーの Phase 1 プロトタイプ。
//!
//! 目的は `ext-session-lock-v1` で全出力をロックできることの確認だけ。
//! 黒一色の `wl_shm` バッファを全 `wl_output` に張り、任意のキー入力で解除して
//! 終了する。認証と UI は Phase 3 で実装する。
//!
//! 実行:
//!   WAYLAND_DISPLAY=wayland-1 cargo run -p nekonolock

#![forbid(unsafe_code)]

use std::time::Duration;

use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{Shm, ShmHandler, raw::RawPool},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
};

fn main() -> Result<()> {
    let conn =
        Connection::connect_to_env().context("Wayland に接続できない（WAYLAND_DISPLAY を確認）")?;

    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh: QueueHandle<App> = event_queue.handle();
    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().context("calloop の event loop を作れない")?;

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor が無い")?;
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let shm = Shm::bind(&globals, &qh).context("wl_shm が無い")?;
    let session_lock_state = SessionLockState::new(&globals, &qh);

    // キーボードは SCTK の seat 実装（xkbcommon 必須）ではなく、生の wl_seat /
    // wl_keyboard で受ける。今回はキーの内容を一切見ないためキーマップは不要。
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ())
        .map_err(|err| anyhow::anyhow!("wl_seat を bind できない: {err}"))
        .ok();

    let mut app = App {
        conn: conn.clone(),
        compositor,
        output_state,
        registry_state,
        shm,
        session_lock_state,
        session_lock: None,
        lock_surfaces: Vec::new(),
        keyboard: None,
        _seat: seat,
        locked_once: false,
        exit: false,
        failed: false,
    };

    app.session_lock = Some(
        app.session_lock_state
            .lock(&qh)
            .context("ext-session-lock-v1 を開始できない（コンポジタ非対応？）")?,
    );

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .context("Wayland の event source を登録できない")?;

    while !app.exit {
        event_loop.dispatch(Duration::from_millis(16), &mut app)?;
    }

    // 失敗経路でもロックを残さない（fail closed ではなく、プロトタイプの後始末）。
    if let Some(lock) = app.session_lock.take() {
        lock.unlock();
        let _ = app.conn.roundtrip();
    }

    if app.failed {
        anyhow::bail!("ロックサーフェスの描画に失敗した");
    }
    if !app.locked_once {
        anyhow::bail!("ロックを取得できなかった");
    }

    println!("done");
    Ok(())
}

struct App {
    conn: Connection,
    compositor: CompositorState,
    output_state: OutputState,
    registry_state: RegistryState,
    shm: Shm,
    session_lock_state: SessionLockState,
    session_lock: Option<SessionLock>,
    lock_surfaces: Vec<(wl_output::WlOutput, String, SessionLockSurface)>,
    _seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    locked_once: bool,
    exit: bool,
    failed: bool,
}

impl App {
    fn create_lock_surface(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Some(lock) = self.session_lock.as_ref() else {
            return;
        };
        let name = self
            .output_state
            .info(&output)
            .and_then(|info| info.name)
            .unwrap_or_else(|| "unknown".to_string());

        let surface = self.compositor.create_surface(qh);
        let lock_surface = lock.create_lock_surface(surface, &output, qh);
        println!("lock-surface: created {name}");
        self.lock_surfaces.push((output, name, lock_surface));
    }

    fn remove_lock_surface(&mut self, output: &wl_output::WlOutput) {
        let before = self.lock_surfaces.len();
        self.lock_surfaces.retain(|(o, _, _)| o != output);
        if self.lock_surfaces.len() != before {
            println!("lock-surface: removed");
        }
    }

    fn draw_black(
        &mut self,
        qh: &QueueHandle<Self>,
        surface: &SessionLockSurface,
        size: (u32, u32),
    ) {
        let (width, height) = size;
        if width == 0 || height == 0 {
            return;
        }

        let name = self
            .lock_surfaces
            .iter()
            .find(|(_, _, lock_surface)| lock_surface.wl_surface() == surface.wl_surface())
            .map(|(_, name, _)| name.clone())
            .unwrap_or_else(|| "unknown".to_string());

        if let Err(err) = self.try_draw_black(qh, surface, width, height) {
            eprintln!("描画に失敗した: {err:#}");
            self.failed = true;
            self.exit = true;
            return;
        }

        println!("lock-surface: {name} {width}x{height}");
    }

    fn try_draw_black(
        &mut self,
        qh: &QueueHandle<Self>,
        surface: &SessionLockSurface,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let stride = width as usize * 4;
        let len = stride * height as usize;

        let mut pool = RawPool::new(len, &self.shm)?;
        fill_black(&mut pool.mmap()[..len]);
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            (),
            qh,
        );

        surface.wl_surface().attach(Some(&buffer), 0, 0);
        surface
            .wl_surface()
            .damage(0, 0, width as i32, height as i32);
        surface.wl_surface().commit();
        buffer.destroy();

        Ok(())
    }

    fn on_key_pressed(&mut self) {
        let Some(lock) = self.session_lock.as_ref() else {
            return;
        };
        if !lock.is_locked() {
            return;
        }

        println!("key-pressed");

        // Phase 3 で認証を実装するまで、既定ビルドはキー入力を無視する。
        // Phase 1 の検証に限り `insecure-test-unlock` feature で解除を有効にする。
        if !cfg!(feature = "insecure-test-unlock") {
            println!("key-ignored: authentication is not implemented");
            return;
        }

        lock.unlock();
        println!("unlocked");
        // unlock_and_destroy をコンポジタへ確実に届けてから終了する。
        let _ = self.conn.roundtrip();
        self.exit = true;
    }
}

/// ARGB8888 (little-endian) のバッファを黒で埋める。
fn fill_black(buf: &mut [u8]) {
    for pixel in buf.as_chunks_mut::<4>().0 {
        *pixel = [0x00, 0x00, 0x00, 0xff];
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, session_lock: SessionLock) {
        println!("locked");
        self.locked_once = true;
        self.session_lock = Some(session_lock);

        let outputs: Vec<_> = self.output_state.outputs().collect();
        for output in outputs {
            self.create_lock_surface(qh, output);
        }
    }

    fn finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session_lock: SessionLock,
    ) {
        println!("finished");
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        self.draw_black(qh, &surface, configure.new_size);
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self
            .session_lock
            .as_ref()
            .is_some_and(SessionLock::is_locked)
        {
            self.create_lock_surface(qh, output);
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.remove_lock_surface(&output);
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let WEnum::Value(capabilities) = capabilities else {
            return;
        };
        if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(proxy.get_keyboard(qh, ()));
            println!("keyboard: ready");
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            state.on_key_pressed();
        }
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // 念のため、ロックしたまま終了しない。
        if let Some(lock) = self.session_lock.take() {
            lock.unlock();
        }
    }
}

smithay_client_toolkit::delegate_compositor!(App);
smithay_client_toolkit::delegate_output!(App);
smithay_client_toolkit::delegate_session_lock!(App);
smithay_client_toolkit::delegate_shm!(App);
smithay_client_toolkit::delegate_registry!(App);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);

#[cfg(test)]
mod tests {
    use super::fill_black;

    #[test]
    fn fill_black_writes_argb8888() {
        let mut buf = [0xcc; 16];
        fill_black(&mut buf);
        assert!(
            buf.as_chunks::<4>()
                .0
                .iter()
                .all(|px| *px == [0x00, 0x00, 0x00, 0xff])
        );
    }
}
