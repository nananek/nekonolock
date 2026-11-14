//! `nekonolock`: Sway 用スクリーンロッカー。
//!
//! `ext-session-lock-v1` で全出力をロックし、`nekonolock-auth` に認証を委ねる。
//! UI は次の 3 層だけを描く。
//!
//! - 外周のモード色フレーム（太さはモードで変える）
//! - 上部 20% のモード帯（`YubiKey PIN` / `Password`）
//! - 中央の時計、入力ドット、状態メッセージ
//!
//! 認証が成功するまで `SessionLock::unlock` を呼ばない（fail closed）。
//!
//! 実行:
//!   WAYLAND_DISPLAY=wayland-1 cargo run -p nekonolock

#![forbid(unsafe_code)]

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nekonolock::auth::{self, Outcome};
use nekonolock::clock;
use nekonolock::input::UiState;
use nekonolock::render::{Renderer, Theme, View};
use nekonolock_proto::{Config, default_socket_path};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RepeatInfo},
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
};

const TICK: Duration = Duration::from_millis(250);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    if let Err(err) = run() {
        eprintln!("nekonolock: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };

    let config = Config::load_ui(args.config.as_deref()).context("設定を読み込めない")?;
    if !clock::validate_format(&config.ui.clock_format) {
        bail!(
            "設定が不正: [ui] clock_format を解釈できない: {}",
            config.ui.clock_format
        );
    }
    let theme =
        Theme::from_config(&config.ui).map_err(|err| anyhow::anyhow!("設定が不正: {err}"))?;
    let renderer = Renderer::load(Path::new(&config.ui.font))
        .map_err(|err| anyhow::anyhow!("ロック画面を描けない: {err}"))?;
    let socket = env::var_os("NEKONOLOCK_SOCKET")
        .map(PathBuf::from)
        .unwrap_or(default_socket_path()?);
    let auth_timeout = Duration::from_secs(config.auth.timeout_sec.clamp(1, 600));

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
    let seat_state = SeatState::new(&globals, &qh);
    let session_lock_state = SessionLockState::new(&globals, &qh);

    let clock_text = clock::format_now(&config.ui.clock_format);
    let status_line = String::new();

    let mut app = App {
        conn: conn.clone(),
        compositor,
        output_state,
        registry_state,
        shm,
        seat_state,
        session_lock_state,
        renderer,
        theme,
        ui: UiState::new(&config.ui, &config.auth),
        socket,
        status_timeout: STATUS_TIMEOUT.min(auth_timeout),
        auth_timeout,
        session_lock: None,
        surfaces: Vec::new(),
        keyboard: None,
        modifiers: Modifiers::default(),
        repeat_info: RepeatInfo::Disable,
        repeat: None,
        pending: None,
        clock_format: config.ui.clock_format.clone(),
        clock_text,
        status_line,
        dirty: true,
        locked_once: false,
        unlocked: false,
        exit: false,
        aborted: None,
    };

    app.session_lock = Some(
        app.session_lock_state
            .lock(&qh)
            .context("ext-session-lock-v1 を開始できない（コンポジタ非対応？）")?,
    );
    app.queue_status();

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .context("Wayland の event source を登録できない")?;

    while !app.exit {
        match event_loop.dispatch(TICK, &mut app) {
            Ok(()) => {}
            Err(err) => {
                app.aborted = Some(format!("Wayland のイベント処理に失敗した: {err}"));
                break;
            }
        }
        app.poll_pending();
        let now = Instant::now();
        if app.tick(now) {
            app.dirty = true;
        }
        if app.dirty {
            app.redraw_all();
            app.dirty = false;
        }
    }

    // 認証成功以外の経路で抜けた場合も、プロセスを終える前にロックを解放する。
    if let Some(lock) = app.session_lock.take() {
        lock.unlock();
        let _ = app.conn.roundtrip();
    }

    if let Some(reason) = app.aborted.take() {
        bail!(reason);
    }
    if !app.locked_once {
        bail!("ロックを取得できなかった");
    }
    if !app.unlocked {
        bail!("認証せずにロックを終了しようとした");
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
    seat_state: SeatState,
    session_lock_state: SessionLockState,
    renderer: Renderer,
    theme: Theme,
    ui: UiState,
    socket: PathBuf,
    status_timeout: Duration,
    auth_timeout: Duration,
    session_lock: Option<SessionLock>,
    surfaces: Vec<LockSurface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,
    repeat_info: RepeatInfo,
    repeat: Option<Repeat>,
    pending: Option<Pending>,
    clock_format: String,
    clock_text: String,
    status_line: String,
    dirty: bool,
    locked_once: bool,
    unlocked: bool,
    exit: bool,
    aborted: Option<String>,
}

struct LockSurface {
    output: wl_output::WlOutput,
    name: String,
    surface: SessionLockSurface,
    pool: Option<SlotPool>,
    size: (u32, u32),
}

struct Pending {
    rx: Receiver<Outcome>,
}

/// 押しっぱなしのキーのリピート。calloop のタイマーを使わず、tick で進める。
struct Repeat {
    event: KeyEvent,
    next: Instant,
    gap: Duration,
}

impl App {
    fn queue_status(&mut self) {
        if self.pending.is_some() || self.ui.is_busy() {
            return;
        }
        self.ui.begin_status();
        self.dirty = true;

        let socket = self.socket.clone();
        let timeout = self.status_timeout;
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("nekonolock-auth-status".to_string())
            .spawn(move || {
                let _ = tx.send(auth::status(&socket, timeout));
            });
        match spawned {
            Ok(_) => self.pending = Some(Pending { rx }),
            Err(err) => {
                eprintln!("nekonolock: 認証スレッドを起動できない: {err}");
                self.ui.helper_unavailable();
            }
        }
    }

    fn submit(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let Some((kind, secret)) = self.ui.take_request(Instant::now()) else {
            self.dirty = true;
            return;
        };
        self.dirty = true;

        let socket = self.socket.clone();
        let timeout = self.auth_timeout;
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("nekonolock-auth-unlock".to_string())
            .spawn(move || {
                let _ = tx.send(auth::unlock(&socket, kind, secret, timeout));
            });
        match spawned {
            Ok(_) => self.pending = Some(Pending { rx }),
            Err(err) => {
                eprintln!("nekonolock: 認証スレッドを起動できない: {err}");
                self.ui.helper_unavailable();
            }
        }
    }

    fn poll_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match pending.rx.try_recv() {
            Ok(outcome) => self.handle_outcome(outcome),
            Err(TryRecvError::Empty) => self.pending = Some(pending),
            Err(TryRecvError::Disconnected) => self.handle_outcome(Outcome::Unavailable(
                "認証スレッドが結果を返さずに終了した".to_string(),
            )),
        }
    }

    fn handle_outcome(&mut self, outcome: Outcome) {
        self.dirty = true;
        match outcome {
            Outcome::Status { card, methods } => {
                self.ui.apply_status(card, &methods);
            }
            Outcome::Unlocked(method) => {
                println!("unlocked: {}", method.as_str());
                self.unlock_and_exit();
            }
            Outcome::Failed(reason) => {
                eprintln!("nekonolock: 認証失敗 reason={}", reason.as_str());
                self.ui.finish_failure(reason, Instant::now());
            }
            Outcome::Unavailable(err) => {
                eprintln!("nekonolock: 認証ヘルパを利用できない: {err}");
                self.ui.helper_unavailable();
            }
        }
    }

    fn unlock_and_exit(&mut self) {
        self.unlocked = true;
        if let Some(lock) = self.session_lock.take() {
            lock.unlock();
            // unlock をコンポジタへ確実に届けてから終了する。
            let _ = self.conn.roundtrip();
        }
        self.exit = true;
    }

    fn tick(&mut self, now: Instant) -> bool {
        let clock = clock::format_now(&self.clock_format);
        let status = self.ui.status_line(now);
        let mut changed = self.advance_repeat(now);
        if clock != self.clock_text {
            self.clock_text = clock;
            changed = true;
        }
        if status != self.status_line {
            self.status_line = status;
            changed = true;
        }
        changed
    }

    fn schedule_repeat(&mut self, event: &KeyEvent) {
        self.repeat = None;
        let RepeatInfo::Repeat { rate, delay } = self.repeat_info else {
            return;
        };
        if !is_repeatable(event) {
            return;
        }
        let gap = Duration::from_micros(1_000_000 / rate.get() as u64);
        self.repeat = Some(Repeat {
            event: event.clone(),
            next: Instant::now() + Duration::from_millis(delay as u64),
            gap,
        });
    }

    fn release_repeat(&mut self, event: &KeyEvent) {
        if self
            .repeat
            .as_ref()
            .is_some_and(|repeat| repeat.event.raw_code == event.raw_code)
        {
            self.repeat = None;
        }
    }

    fn advance_repeat(&mut self, now: Instant) -> bool {
        let Some(mut repeat) = self.repeat.take() else {
            return false;
        };
        let mut changed = false;
        let mut count = 0;
        while now >= repeat.next && count < 20 {
            let event = repeat.event.clone();
            repeat.next += repeat.gap;
            self.on_key(event);
            changed = true;
            count += 1;
        }
        self.repeat = Some(repeat);
        changed
    }

    fn redraw_all(&mut self) {
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].size.0 == 0 || self.surfaces[index].size.1 == 0 {
                continue;
            }
            if let Err(err) = self.draw_surface(index) {
                // 描画に失敗してもロックは維持する。次の configure や
                // 時計の更新でもう一度試す。
                eprintln!("nekonolock: 描画に失敗した: {err:#}");
            }
        }
    }

    fn draw_surface(&mut self, index: usize) -> Result<()> {
        let (width, height) = self.surfaces[index].size;
        let stride = width as usize * 4;
        let surface = self.surfaces[index].surface.wl_surface().clone();

        let pool = self.surfaces[index]
            .pool
            .as_mut()
            .context("描画用の pool が無い")?;
        let (buffer, canvas) = pool
            .create_buffer(
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            )
            .context("wl_shm のバッファを作れない")?;

        let view = View {
            mode: self.ui.mode(),
            secret_len: self.ui.secret_len(),
            clock: &self.clock_text,
            message: &self.status_line,
        };
        self.renderer
            .render(canvas, stride, width, height, &self.theme, &view);

        buffer
            .attach_to(&surface)
            .context("バッファを surface に attach できない")?;
        surface.damage(0, 0, width as i32, height as i32);
        surface.commit();
        Ok(())
    }

    fn on_configure(&mut self, surface: &SessionLockSurface, size: (u32, u32)) -> Result<()> {
        let Some(index) = self
            .surfaces
            .iter()
            .position(|entry| entry.surface.wl_surface() == surface.wl_surface())
        else {
            return Ok(());
        };

        if size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        if self.surfaces[index].size != size || self.surfaces[index].pool.is_none() {
            let len = size.0 as usize * size.1 as usize * 4;
            self.surfaces[index].pool =
                Some(SlotPool::new(len, &self.shm).context("wl_shm の pool を作れない")?);
            self.surfaces[index].size = size;
        }
        self.draw_surface(index)?;
        let name = self.surfaces[index].name.clone();
        println!("lock-surface: {name} {}x{}", size.0, size.1);
        Ok(())
    }

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
        self.surfaces.push(LockSurface {
            output,
            name,
            surface: lock_surface,
            pool: None,
            size: (0, 0),
        });
    }

    fn remove_lock_surface(&mut self, output: &wl_output::WlOutput) {
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|entry| entry.output == *output)
        {
            let entry = self.surfaces.remove(index);
            let name = entry.name.clone();
            // ext_session_lock_surface_v1 を先に destroy し、その後 wl_surface を消す。
            let wl_surface = entry.surface.wl_surface().clone();
            drop(entry.surface);
            wl_surface.destroy();
            println!("lock-surface: removed {name}");
        }
    }

    fn on_key(&mut self, event: KeyEvent) {
        // 修飾キー付きの入力はテキストとして扱わない（Ctrl+C が 'c' に化けるのを防ぐ）。
        if self.modifiers.ctrl || self.modifiers.alt || self.modifiers.logo {
            return;
        }

        match event.keysym {
            Keysym::Escape => {
                self.ui.clear_secret();
                self.dirty = true;
            }
            Keysym::Return | Keysym::KP_Enter => self.submit(),
            Keysym::BackSpace => {
                if self.ui.backspace() {
                    self.dirty = true;
                }
            }
            Keysym::Tab => {
                if self.ui.toggle_mode() {
                    self.dirty = true;
                }
            }
            _ => {
                if let Some(text) = event.utf8 {
                    let mut changed = false;
                    for c in text.chars() {
                        changed |= self.ui.push_char(c);
                    }
                    if changed {
                        self.dirty = true;
                    }
                }
            }
        }
    }
}

/// 押しっぱなしで繰り返してよいキー。Enter / Tab / Esc は繰り返さない。
fn is_repeatable(event: &KeyEvent) -> bool {
    matches!(event.keysym, Keysym::BackSpace)
        || (event.utf8.is_some()
            && !matches!(
                event.keysym,
                Keysym::Return | Keysym::KP_Enter | Keysym::Tab | Keysym::Escape
            ))
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
        self.session_lock = None;
        if !self.unlocked {
            self.aborted = Some("コンポジタ側でロックが終了した".to_string());
        }
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        if let Err(err) = self.on_configure(&surface, configure.new_size) {
            eprintln!("nekonolock: configure を処理できない: {err:#}");
        }
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

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => {
                    self.keyboard = Some(keyboard);
                    println!("keyboard: ready");
                }
                Err(err) => eprintln!("nekonolock: キーボードを取得できない: {err}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
            self.repeat = None;
            self.modifiers = Modifiers::default();
            println!("keyboard: released");
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        println!("keyboard: enter");
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        println!("keyboard: leave");
        self.repeat = None;
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.schedule_repeat(&event);
        self.on_key(event);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.release_repeat(&event);
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _layout: u32,
    ) {
        self.modifiers = modifiers;
    }

    fn update_repeat_info(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        info: RepeatInfo,
    ) {
        self.repeat_info = info;
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // 正常終了以外でもロックを残したままプロセスを消さない。
        if let Some(lock) = self.session_lock.take() {
            lock.unlock();
        }
    }
}

smithay_client_toolkit::delegate_compositor!(App);
smithay_client_toolkit::delegate_output!(App);
smithay_client_toolkit::delegate_seat!(App);
smithay_client_toolkit::delegate_keyboard!(App);
smithay_client_toolkit::delegate_session_lock!(App);
smithay_client_toolkit::delegate_shm!(App);
smithay_client_toolkit::delegate_registry!(App);

fn parse_args() -> Result<Option<Args>> {
    let mut args = env::args_os();
    let program = args.next().unwrap_or_else(|| OsString::from("nekonolock"));

    let mut config = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--config") => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config にパスが続かない"))?;
                config = Some(PathBuf::from(value));
            }
            Some("-h" | "--help") => {
                println!(
                    "使い方: {} [--config PATH]\n\n\
                     --config  config.toml（既定: $XDG_CONFIG_HOME/nekonolock/config.toml）",
                    PathBuf::from(program).display()
                );
                return Ok(None);
            }
            Some(other) => bail!("知らない引数: {other}"),
            None => bail!("引数を UTF-8 として解釈できない"),
        }
    }

    Ok(Some(Args { config }))
}

struct Args {
    config: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(keysym: Keysym, utf8: Option<&str>) -> KeyEvent {
        KeyEvent {
            time: 0,
            raw_code: 0,
            keysym,
            utf8: utf8.map(str::to_string),
        }
    }

    #[test]
    fn enter_tab_and_escape_are_not_repeated() {
        assert!(!is_repeatable(&event(Keysym::Return, Some("\r"))));
        assert!(!is_repeatable(&event(Keysym::KP_Enter, Some("\r"))));
        assert!(!is_repeatable(&event(Keysym::Tab, Some("\t"))));
        assert!(!is_repeatable(&event(Keysym::Escape, None)));
    }

    #[test]
    fn backspace_and_text_are_repeated() {
        assert!(is_repeatable(&event(Keysym::BackSpace, None)));
        assert!(is_repeatable(&event(Keysym::a, Some("a"))));
        assert!(is_repeatable(&event(Keysym::_1, Some("1"))));
    }
}
