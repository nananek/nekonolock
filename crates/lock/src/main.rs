//! `nekonolock`: Sway 用スクリーンロッカー。
//!
//! `ext-session-lock-v1` で全出力をロックし、`nekonolock-auth` に認証を委ねる。
//! UI は次の 3 層だけを描く。
//!
//! - 上端の accent line
//! - 左上ラベル、右上 `LOCKED`、時計、日付
//! - モード capsule（`YUBIKEY PIN` / `PASSWORD`）、入力欄、状態メッセージ
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
use nekonolock_proto::{AuthConfig, Config, UiConfig, default_socket_path};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers, RepeatInfo},
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
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
};

const TICK: Duration = Duration::from_millis(250);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// カードがある間の status 問い合わせ間隔。抜去を 1 秒以内に画面へ反映する。
/// `gpg --card-status` は scdaemon とカードに触れるため、これ以上短くしない。
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// カードが無いと分かっている間の問い合わせ間隔。挿入の検知は急がない。
const STATUS_INTERVAL_ABSENT: Duration = Duration::from_secs(10);

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

    let default_ui = UiConfig::default();
    let default_clock_format = default_ui.clock_format.clone();
    let default_date_format = default_ui.date_format.clone();
    let clock_text = clock::format_now(&default_clock_format);
    let date_text = clock::format_now(&default_date_format);

    let mut app = App {
        conn: conn.clone(),
        compositor,
        output_state,
        registry_state,
        shm,
        seat_state,
        session_lock_state,
        renderer: Renderer::fallback(),
        theme: Theme::fallback(),
        ui: UiState::new(&default_ui, &AuthConfig::default()),
        config_path: args.config,
        socket: None,
        status_timeout: STATUS_TIMEOUT,
        auth_timeout: Duration::from_secs(60),
        last_status: Instant::now(),
        session_lock: None,
        surfaces: Vec::new(),
        keyboard: None,
        modifiers: Modifiers::default(),
        repeat_info: RepeatInfo::Disable,
        repeat: None,
        pending: None,
        clock_format: default_clock_format,
        clock_text,
        date_format: default_date_format,
        date_text,
        label: default_ui.label.clone(),
        blank_after: Duration::from_secs(default_ui.blank_after_sec),
        blanked: false,
        last_activity: Instant::now(),
        status_line: String::new(),
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

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .context("Wayland の event source を登録できない")?;

    // 設定・フォントの読み込みより先に、フォールバック描画でロックを成立させる。
    // 読み込みが遅い/失敗しても画面が覆われない状態を作らない。
    let deadline = Instant::now() + Duration::from_secs(2);
    while !app.exit && app.aborted.is_none() && !app.painted() && Instant::now() < deadline {
        match event_loop.dispatch(Duration::from_millis(100), &mut app) {
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

    if !app.exit && app.aborted.is_none() {
        app.load_runtime_config();
    }

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
    config_path: Option<PathBuf>,
    socket: Option<PathBuf>,
    status_timeout: Duration,
    auth_timeout: Duration,
    last_status: Instant,
    session_lock: Option<SessionLock>,
    surfaces: Vec<LockSurface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,
    repeat_info: RepeatInfo,
    repeat: Option<Repeat>,
    pending: Option<Pending>,
    clock_format: String,
    clock_text: String,
    date_format: String,
    date_text: String,
    label: String,
    blank_after: Duration,
    blanked: bool,
    last_activity: Instant,
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
    /// configure で来る論理サイズ。バッファはこれに `scale` を掛ける。
    size: (u32, u32),
    scale: i32,
}

struct Pending {
    rx: Receiver<Outcome>,
    /// 定期確認の status。応答は助言として扱い、ユーザーの操作を待たせない。
    probe: bool,
}

/// 押しっぱなしのキーのリピート。calloop のタイマーを使わず、tick で進める。
struct Repeat {
    event: KeyEvent,
    next: Instant,
    gap: Duration,
}

impl App {
    /// フォールバック描画で少なくとも 1 つのロックサーフェスが塗れたか。
    fn painted(&self) -> bool {
        self.surfaces
            .iter()
            .any(|surface| surface.size.0 > 0 && surface.size.1 > 0 && surface.pool.is_some())
    }

    /// ロック取得後に設定とフォントを読む。失敗しても既定値でロックを維持する。
    fn load_runtime_config(&mut self) {
        let mut config_error = false;
        let config = match Config::load_ui(self.config_path.as_deref()) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("nekonolock: 設定を読み込めない（既定値でロックを維持）: {err:#}");
                config_error = true;
                Config::default()
            }
        };

        if clock::validate_format(&config.ui.clock_format) {
            self.clock_format = config.ui.clock_format.clone();
        } else {
            eprintln!(
                "nekonolock: [ui] clock_format を解釈できない（既定値を使う）: {}",
                config.ui.clock_format
            );
        }
        if clock::validate_format(&config.ui.date_format) {
            self.date_format = config.ui.date_format.clone();
        } else {
            eprintln!(
                "nekonolock: [ui] date_format を解釈できない（既定値を使う）: {}",
                config.ui.date_format
            );
        }

        match Theme::from_config(&config.ui) {
            Ok(theme) => self.theme = theme,
            Err(err) => eprintln!("nekonolock: 表示色を読めない（既定値を使う）: {err}"),
        }

        self.ui = UiState::new(&config.ui, &config.auth);
        if config_error {
            self.ui.config_error();
        }
        self.auth_timeout = Duration::from_secs(config.auth.timeout_sec.clamp(1, 600));
        self.status_timeout = STATUS_TIMEOUT.min(self.auth_timeout);
        self.label = config.ui.label.clone();
        self.blank_after = Duration::from_secs(config.ui.blank_after_sec.min(86_400));
        self.last_activity = Instant::now();
        self.clock_text = clock::format_now(&self.clock_format);
        self.date_text = clock::format_now(&self.date_format);
        self.load_socket();

        let clock_font = if config.ui.clock_font.trim().is_empty() {
            None
        } else {
            Some(Path::new(&config.ui.clock_font))
        };
        match Renderer::load(Path::new(&config.ui.font), clock_font) {
            Ok(renderer) => self.renderer = renderer,
            Err(err) => {
                eprintln!("nekonolock: フォントを読めない（文字なしでロックを維持）: {err}")
            }
        }

        self.dirty = true;
        if !self.exit {
            self.queue_status();
        }
    }

    fn load_socket(&mut self) {
        let socket = env::var_os("NEKONOLOCK_SOCKET")
            .map(PathBuf::from)
            .or_else(|| default_socket_path().ok());
        if socket.is_none() {
            eprintln!("nekonolock: socket のパスを決められない（XDG_RUNTIME_DIR を確認）");
        }
        self.socket = socket;
    }

    fn queue_status(&mut self) {
        self.queue_status_with(true);
    }

    /// status を問い合わせる。`announce` が false のときは入力中でも邪魔をせず、
    /// カード抜去の検知だけを行う。
    fn queue_status_with(&mut self, announce: bool) {
        let Some(socket) = self.socket.clone() else {
            self.ui.helper_unavailable();
            self.dirty = true;
            return;
        };
        if self.pending.is_some() {
            return;
        }
        if announce {
            if self.ui.is_busy() {
                return;
            }
            self.ui.begin_status();
        }
        self.last_status = Instant::now();
        self.dirty = true;

        let timeout = self.status_timeout;
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("nekonolock-auth-status".to_string())
            .spawn(move || {
                let _ = tx.send(auth::status(&socket, timeout));
            });
        match spawned {
            Ok(_) => {
                self.pending = Some(Pending {
                    rx,
                    probe: !announce,
                })
            }
            Err(err) => {
                eprintln!("nekonolock: 認証スレッドを起動できない: {err}");
                if announce {
                    self.ui.helper_unavailable();
                }
            }
        }
    }

    /// ロック中にカードが抜けた場合に備えて status を問い合わせる。
    ///
    /// カードがある間は 1 秒ごとに確認し、無いと分かった後は間隔を空ける。
    fn poll_card_status(&mut self, now: Instant) {
        if self.pending.is_some()
            || self.ui.is_busy()
            || now.duration_since(self.last_status) < status_interval(self.ui.card())
        {
            return;
        }
        self.queue_status_with(false);
    }

    fn submit(&mut self) {
        let Some(socket) = self.socket.clone() else {
            self.ui.helper_unavailable();
            self.dirty = true;
            return;
        };
        match &self.pending {
            // 認証の応答待ちなら二重送信しない。定期確認の応答待ちは捨てて
            // 送信を優先する（Enter を取りこぼすとキーが効いていないように見える）。
            Some(pending) if !pending.probe => return,
            Some(_) | None => {}
        }
        let Some((kind, secret)) = self.ui.take_request(Instant::now()) else {
            self.dirty = true;
            return;
        };
        self.pending = None;
        self.dirty = true;

        let timeout = self.auth_timeout;
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("nekonolock-auth-unlock".to_string())
            .spawn(move || {
                let _ = tx.send(auth::unlock(&socket, kind, secret, timeout));
            });
        match spawned {
            Ok(_) => self.pending = Some(Pending { rx, probe: false }),
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
            Ok(outcome) => {
                // 定期確認の失敗で入力中の秘密を消さない。送信時に改めて知らせる。
                if pending.probe && !matches!(outcome, Outcome::Status { .. }) {
                    eprintln!("nekonolock: 定期確認に失敗した（表示は変えない）");
                    return;
                }
                self.handle_outcome(outcome);
            }
            Err(TryRecvError::Empty) => self.pending = Some(pending),
            Err(TryRecvError::Disconnected) if pending.probe => {
                eprintln!("nekonolock: 定期確認のスレッドが結果を返さずに終了した");
            }
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
        // 成功経路でも入力欄の秘密を残さない。
        self.ui.clear_secret();
        if let Some(lock) = self.session_lock.take() {
            lock.unlock();
            // unlock をコンポジタへ確実に届けてから終了する。
            let _ = self.conn.roundtrip();
        }
        self.exit = true;
    }

    fn tick(&mut self, now: Instant) -> bool {
        // 純黒の間は時計も状態も更新しない。入力が来たら on_key が起こす。
        if self.blanked {
            return false;
        }

        let clock = clock::format_now(&self.clock_format);
        let date = clock::format_now(&self.date_format);
        let status = self.ui.status_line(now);
        let mut changed = self.advance_repeat(now);
        self.poll_card_status(now);
        if clock != self.clock_text {
            self.clock_text = clock;
            changed = true;
        }
        if date != self.date_text {
            self.date_text = date;
            changed = true;
        }
        if status != self.status_line {
            self.status_line = status;
            changed = true;
        }

        // 認証待ちの間に黒くしない。応答が来たら通常の描画に戻す。
        if blank_due(
            now,
            self.last_activity,
            self.blank_after,
            self.pending.is_some(),
        ) {
            self.blanked = true;
            self.repeat = None;
            // 途中まで入れた秘密を黒画面の背後に残さない。
            self.ui.clear_secret();
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
        let (logical_w, logical_h) = self.surfaces[index].size;
        if logical_w == 0 || logical_h == 0 {
            return Ok(());
        }
        let scale = self.surfaces[index].scale.max(1) as u32;
        let width = logical_w * scale;
        let height = logical_h * scale;
        let stride = width as usize * 4;
        let surface = self.surfaces[index].surface.wl_surface().clone();
        surface.set_buffer_scale(scale as i32);

        if self.surfaces[index].pool.is_none() {
            let len = width as usize * height as usize * 4;
            let pool = SlotPool::new(len, &self.shm).context("wl_shm の pool を作れない")?;
            self.surfaces[index].pool = Some(pool);
        }

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
            date: &self.date_text,
            message: &self.status_line,
            label: &self.label,
            blank: self.blanked,
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
        if self.surfaces[index].size != size {
            self.surfaces[index].size = size;
            self.surfaces[index].pool = None;
        }
        self.draw_surface(index)?;
        let name = self.surfaces[index].name.clone();
        let scale = self.surfaces[index].scale.max(1) as u32;
        println!(
            "lock-surface: {name} {}x{} scale={scale} buffer={}x{}",
            size.0,
            size.1,
            size.0 * scale,
            size.1 * scale
        );
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
        let scale = self
            .output_state
            .info(&output)
            .map(|info| info.scale_factor)
            .unwrap_or(1)
            .max(1);

        let surface = self.compositor.create_surface(qh);
        surface.set_buffer_scale(scale);
        let lock_surface = lock.create_lock_surface(surface, &output, qh);
        println!("lock-surface: created {name} scale={scale}");
        self.surfaces.push(LockSurface {
            output,
            name,
            surface: lock_surface,
            pool: None,
            size: (0, 0),
            scale,
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
        self.last_activity = Instant::now();
        if self.blanked {
            // 黒画面からの復帰キーは入力として消費する。見えない入力欄に
            // 秘密の 1 文字目が入るのを避ける。
            self.blanked = false;
            self.repeat = None;
            self.clock_text = clock::format_now(&self.clock_format);
            self.date_text = clock::format_now(&self.date_format);
            self.dirty = true;
            return;
        }

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

/// カードの有無に応じた status の問い合わせ間隔。
///
/// 一度抜けた後は、挿し直しをすぐ PIN モードへ戻さない設計なので急がない。
fn status_interval(card: Option<bool>) -> Duration {
    if card == Some(false) {
        STATUS_INTERVAL_ABSENT
    } else {
        STATUS_INTERVAL
    }
}

/// 無入力タイマーが満了したか。認証待ちの間は黒くしない。
fn blank_due(now: Instant, last_activity: Instant, blank_after: Duration, pending: bool) -> bool {
    !blank_after.is_zero() && !pending && now.duration_since(last_activity) >= blank_after
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
        self.ui.clear_secret();
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
        output: wl_output::WlOutput,
    ) {
        let scale = self
            .output_state
            .info(&output)
            .map(|info| info.scale_factor)
            .unwrap_or(1)
            .max(1);
        let mut changed = false;
        for entry in &mut self.surfaces {
            if entry.output == output && entry.scale != scale {
                entry.scale = scale;
                entry.pool = None;
                entry.surface.wl_surface().set_buffer_scale(scale);
                changed = true;
            }
        }
        if changed {
            self.dirty = true;
        }
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

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
        // キーリピートは tick 側の Repeat で進めるため、compositor からの
        // repeat は使わない。
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
        _raw_modifiers: RawModifiers,
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

smithay_client_toolkit::delegate_dispatch2!(App);
smithay_client_toolkit::delegate_registry!(App);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);

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

    #[test]
    fn blank_timer_respects_zero_and_pending() {
        let now = Instant::now();
        let earlier = now.checked_sub(Duration::from_secs(10)).unwrap();
        assert!(!blank_due(now, earlier, Duration::ZERO, false));
        assert!(!blank_due(now, earlier, Duration::from_secs(5), true));
        assert!(blank_due(now, earlier, Duration::from_secs(5), false));
        assert!(!blank_due(
            now,
            now.checked_sub(Duration::from_secs(1)).unwrap(),
            Duration::from_secs(5),
            false
        ));
    }

    #[test]
    fn status_interval_is_short_while_the_card_is_present() {
        assert_eq!(status_interval(None), Duration::from_secs(1));
        assert_eq!(status_interval(Some(true)), Duration::from_secs(1));
        assert_eq!(status_interval(Some(false)), Duration::from_secs(10));
    }
}
