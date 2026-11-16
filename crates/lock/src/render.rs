//! `wl_shm` の ARGB8888 バッファへ直接描くソフトウェアレンダラ。
//!
//! 既定は濃いチャコールの Ink テーマ。GPU もシェーピングも使わない。
//! 画面は上から次の順に描く。
//!
//! 1. 上端の accent line
//! 2. 左上のラベルと右上の LOCKED
//! 3. 時計と日付
//! 4. モード capsule（色・ドット・ラベル）
//! 5. 入力欄（accent の枠と入力ドット）
//! 6. 案内または状態メッセージ
//! 7. 下端の操作ヒント
//!
//! 固定のピクセル値は使わず、すべてバッファの幅・高さに対する比率で置く。
//! HiDPI ではバッファだけを `wl_output.scale` 倍にし、`set_buffer_scale` で
//! 論理サイズに戻すため、この関数は解像度を意識しない。
//!
//! 文字は既定で ASCII を想定する。CJK を出す場合は `[ui] font` を
//! CJK 対応フォントへ差し替える。

use std::cell::RefCell;
use std::fs;
use std::path::Path;

use fontdue::{Font, FontSettings, Metrics};
use nekonolock_proto::UiConfig;

use crate::input::Mode;

/// 設定で指定された `#rrggbb` 色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hex(value: &str) -> Option<Self> {
        let hex = value.strip_prefix('#')?;
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some(Self { r, g, b })
    }

    /// `self` を `other` に寄せる。`t = 0` で `self`、`t = 1` で `other`。
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let f = |a: u8, b: u8| (a as f32 * (1.0 - t) + b as f32 * t).round() as u8;
        Self {
            r: f(self.r, other.r),
            g: f(self.g, other.g),
            b: f(self.b, other.b),
        }
    }
}

const BLACK: Color = Color::rgb(0, 0, 0);

/// 画面全体で使う色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub background: Color,
    pub background_bottom: Color,
    pub foreground: Color,
    pub dim: Color,
    pub faint: Color,
    pub field_background: Color,
    pub pin_accent: Color,
    pub password_accent: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Color::rgb(0x15, 0x18, 0x1b),
            background_bottom: Color::rgb(0x0b, 0x0d, 0x0f),
            foreground: Color::rgb(0xf2, 0xef, 0xe7),
            dim: Color::rgb(0x9b, 0x9e, 0x9c),
            faint: Color::rgb(0x6d, 0x72, 0x74),
            field_background: Color::rgb(0x1b, 0x1f, 0x22),
            pin_accent: Color::rgb(0x8f, 0xc7, 0xb8),
            password_accent: Color::rgb(0xe0, 0xa1, 0x8d),
        }
    }
}

impl Theme {
    /// 設定を読めないときに使う既定色。ロックを先に取得するために必要。
    pub fn fallback() -> Self {
        Self::default()
    }

    /// 明るい Paper テーマ。README と preview 用。設定では同じ値を
    /// `[ui]` に書けば使える。
    pub fn paper() -> Self {
        Self {
            background: Color::rgb(0xf8, 0xf4, 0xec),
            background_bottom: Color::rgb(0xed, 0xe6, 0xda),
            foreground: Color::rgb(0x1f, 0x1d, 0x1a),
            dim: Color::rgb(0x7b, 0x72, 0x65),
            faint: Color::rgb(0xa7, 0x9c, 0x8c),
            field_background: Color::rgb(0xff, 0xfd, 0xf8),
            pin_accent: Color::rgb(0x2f, 0x5d, 0x62),
            password_accent: Color::rgb(0xa3, 0x4a, 0x3a),
        }
    }

    pub fn from_config(ui: &UiConfig) -> Result<Self, String> {
        Ok(Self {
            background: parse("background", &ui.background)?,
            background_bottom: parse("background_bottom", &ui.background_bottom)?,
            foreground: parse("foreground", &ui.foreground)?,
            dim: parse("dim", &ui.dim)?,
            faint: parse("faint", &ui.faint)?,
            field_background: parse("field_background", &ui.field_background)?,
            pin_accent: parse("pin_accent", &ui.pin_accent)?,
            password_accent: parse("password_accent", &ui.password_accent)?,
        })
    }

    fn accent(self, mode: Mode) -> Color {
        match mode {
            Mode::Pin => self.pin_accent,
            Mode::Password => self.password_accent,
        }
    }

    /// capsule の下地。accent を背景に 16% だけ混ぜる。
    fn tint(self, mode: Mode) -> Color {
        self.background.mix(self.accent(mode), 0.16)
    }
}

fn parse(name: &str, value: &str) -> Result<Color, String> {
    Color::from_hex(value)
        .ok_or_else(|| format!("[ui] {name} を #rrggbb として解釈できない: {value}"))
}

/// 下端の操作ヒント。画面のコピーはすべて大文字に揃える。
const BOTTOM_HINT: &str = "TAB  SWITCH MODE     /     ENTER  UNLOCK";

/// 1 フレーム分の表示内容。
pub struct View<'a> {
    pub mode: Mode,
    pub secret_len: usize,
    pub clock: &'a str,
    pub date: &'a str,
    pub message: &'a str,
    pub label: &'a str,
    /// 焼き付き対策の純黒フレーム。true なら他は一切描かない。
    pub blank: bool,
}

/// フォントを保持するレンダラ。フォントを読めない場合はテキストを描かず、
/// 背景・accent line・capsule・入力欄だけでロックを維持する。
pub struct Renderer {
    font: Option<Font>,
    clock_font: Option<Font>,
    /// 背景グラデーションのキャッシュ。変換のたびに全画素を塗り直さない。
    background: RefCell<Option<BackgroundCache>>,
}

struct BackgroundCache {
    width: u32,
    height: u32,
    theme: Theme,
    data: Vec<u8>,
}

impl Renderer {
    /// `font` は本文用。`clock_font` を読めない場合は本文フォントを使う。
    pub fn load(font: &Path, clock_font: Option<&Path>) -> Result<Self, String> {
        let font = load_font(font)?;
        let clock_font = clock_font.and_then(|path| match load_font(path) {
            Ok(font) => Some(font),
            Err(err) => {
                eprintln!("nekonolock: 時計フォントを読めない（本文フォントを使う）: {err}");
                None
            }
        });
        Ok(Self {
            font: Some(font),
            clock_font,
            background: RefCell::new(None),
        })
    }

    /// フォント無しのフォールバック。ロックを先に取得するために使う。
    pub fn fallback() -> Self {
        Self {
            font: None,
            clock_font: None,
            background: RefCell::new(None),
        }
    }

    fn font(&self) -> Option<&Font> {
        self.font.as_ref()
    }

    fn clock_font(&self) -> Option<&Font> {
        self.clock_font.as_ref().or(self.font.as_ref())
    }

    /// `canvas` は `stride * height` byte 以上の ARGB8888 バッファ。
    pub fn render(
        &self,
        canvas: &mut [u8],
        stride: usize,
        width: u32,
        height: u32,
        theme: &Theme,
        view: &View<'_>,
    ) {
        let width = width as i32;
        let height = height as i32;
        if width <= 0 || height <= 0 || stride < width as usize * 4 {
            return;
        }
        let stride = stride as i32;
        let len = (stride * height) as usize;
        if canvas.len() < len {
            return;
        }
        let canvas = &mut canvas[..len];

        if view.blank {
            fill(canvas, BLACK);
            return;
        }

        // 背景はテーマとサイズが同じ間だけキャッシュする。キー入力のたびに
        // 全画素のグラデーションを計算し直すと、2x バッファでは無視できない。
        let mut background = self.background.borrow_mut();
        let cached = background.as_ref().is_some_and(|cache| {
            cache.width == width as u32 && cache.height == height as u32 && cache.theme == *theme
        });
        if cached {
            if let Some(cache) = background.as_ref() {
                canvas.copy_from_slice(&cache.data);
            }
        } else {
            fill_gradient(
                canvas,
                stride,
                width,
                height,
                theme.background,
                theme.background_bottom,
            );
            let mut data = vec![0u8; len];
            data.copy_from_slice(canvas);
            *background = Some(BackgroundCache {
                width: width as u32,
                height: height as u32,
                theme: *theme,
                data,
            });
        }
        drop(background);

        let w = width as f32;
        let h = height as f32;
        let accent = theme.accent(view.mode);

        // 上端の accent line。モードを画面の一番外側でも示す。
        let line = (h * 0.00243).round().max(1.0) as i32;
        fill_rect(canvas, stride, width, height, 0, 0, width, line, accent);

        // 上 bar: ラベルと LOCKED。
        let margin = w * 0.0365;
        let top_y = h * 0.0584;
        let small = (h * 0.0105).max(8.0);
        let tracking = h * 0.0024;
        if let Some(font) = self.font() {
            let label = view.label.to_uppercase();
            if !label.is_empty() {
                draw_text(
                    font,
                    canvas,
                    stride,
                    width,
                    height,
                    &label,
                    margin,
                    top_y,
                    small,
                    theme.faint,
                    tracking,
                    Align::Left,
                );
            }
            let right = w - margin;
            draw_text(
                font,
                canvas,
                stride,
                width,
                height,
                "LOCKED",
                right,
                top_y,
                small,
                theme.faint,
                tracking,
                Align::Right,
            );
            let locked_w = measure(font, "LOCKED", small, tracking);
            let dot_r = (h * 0.0032).max(1.0);
            fill_circle(
                canvas,
                stride,
                width,
                height,
                right - locked_w - dot_r * 3.0,
                top_y,
                dot_r,
                accent,
            );
        }

        // 時計。
        if let Some(font) = self.clock_font() {
            let size = fit_size(font, view.clock, h * 0.1297, w * 0.72, 0.0);
            draw_text(
                font,
                canvas,
                stride,
                width,
                height,
                view.clock,
                w / 2.0,
                h * 0.3242,
                size,
                theme.foreground,
                0.0,
                Align::Center,
            );
        }

        // 日付。大文字 + tracking で小さく見せる。
        if let Some(font) = self.font() {
            let date = view.date.to_uppercase();
            if !date.is_empty() {
                let tracking = h * 0.0018;
                let size = fit_size(font, &date, h * 0.0211, w * 0.82, tracking);
                draw_text(
                    font,
                    canvas,
                    stride,
                    width,
                    height,
                    &date,
                    w / 2.0,
                    h * 0.4214,
                    size,
                    theme.dim,
                    tracking,
                    Align::Center,
                );
            }
        }

        // モード capsule。
        let cap_h = h * 0.0340;
        let cap_cy = h * 0.4943;
        let pad = w * 0.0091;
        let dot_r = (h * 0.0032).max(1.0);
        let gap = h * 0.0081;
        let label = view.mode.label();
        let cap_w = self
            .font()
            .map(|font| {
                let size = (h * 0.0146).max(8.0);
                measure(font, label, size, 0.0) + pad * 2.0 + dot_r * 2.0 + gap
            })
            .unwrap_or(w * 0.14);
        let cap_x = w / 2.0 - cap_w / 2.0;
        fill_rounded_rect(
            canvas,
            stride,
            width,
            height,
            cap_x,
            cap_cy - cap_h / 2.0,
            cap_w,
            cap_h,
            cap_h / 2.0,
            theme.tint(view.mode),
        );
        fill_circle(
            canvas,
            stride,
            width,
            height,
            cap_x + pad + dot_r,
            cap_cy,
            dot_r,
            accent,
        );
        if let Some(font) = self.font() {
            let size = (h * 0.0146).max(8.0);
            draw_text(
                font,
                canvas,
                stride,
                width,
                height,
                label,
                cap_x + pad + dot_r * 2.0 + gap,
                cap_cy,
                size,
                accent,
                0.0,
                Align::Left,
            );
        }

        // 入力欄。外側を accent、内側を field 色で塗って枠にする。
        let field_w = w * 0.1823;
        let field_h = h * 0.0519;
        let field_cx = w / 2.0;
        let field_cy = h * 0.5592;
        let radius = h * 0.0146;
        let border = (h * 0.0010).max(1.0);
        fill_rounded_rect(
            canvas,
            stride,
            width,
            height,
            field_cx - field_w / 2.0,
            field_cy - field_h / 2.0,
            field_w,
            field_h,
            radius,
            accent,
        );
        let inner_w = (field_w - border * 2.0).max(0.0);
        let inner_h = (field_h - border * 2.0).max(0.0);
        fill_rounded_rect(
            canvas,
            stride,
            width,
            height,
            field_cx - inner_w / 2.0,
            field_cy - inner_h / 2.0,
            inner_w,
            inner_h,
            (radius - border).max(0.0),
            theme.field_background,
        );

        let dot_r = (h * 0.0065).max(1.5);
        let dot_gap = h * 0.0114;
        let max_dots =
            (((inner_w - dot_r * 4.0) / (dot_r * 2.0 + dot_gap)).floor() as usize).max(1);
        let count = view.secret_len.min(max_dots);
        if count == 0 {
            let caret_w = (h * 0.0016).max(1.0);
            let caret_h = (h * 0.0162).max(2.0);
            fill_rounded_rect(
                canvas,
                stride,
                width,
                height,
                field_cx - caret_w / 2.0,
                field_cy - caret_h / 2.0,
                caret_w,
                caret_h,
                caret_w / 2.0,
                theme.faint,
            );
        } else {
            let total = count as f32 * (dot_r * 2.0) + (count.saturating_sub(1)) as f32 * dot_gap;
            let start = field_cx - total / 2.0 + dot_r;
            for index in 0..count {
                fill_circle(
                    canvas,
                    stride,
                    width,
                    height,
                    start + index as f32 * (dot_r * 2.0 + dot_gap),
                    field_cy,
                    dot_r,
                    accent,
                );
            }
        }

        // 状態メッセージ。無ければモードごとの案内。どちらも大文字で出す。
        if let Some(font) = self.font() {
            let (text, color) = if view.message.is_empty() {
                (view.mode.hint().to_string(), theme.dim)
            } else {
                (view.message.to_uppercase(), theme.foreground)
            };
            let size = fit_size(font, &text, h * 0.0146, w * 0.82, 0.0);
            draw_text(
                font,
                canvas,
                stride,
                width,
                height,
                &text,
                w / 2.0,
                h * 0.6402,
                size,
                color,
                0.0,
                Align::Center,
            );

            // 下端の操作ヒント。小さく、でも読み取れる濃さにする。
            let tracking = h * 0.0010;
            let size = fit_size(font, BOTTOM_HINT, h * 0.0105, w * 0.9, tracking);
            draw_text(
                font,
                canvas,
                stride,
                width,
                height,
                BOTTOM_HINT,
                w / 2.0,
                h * 0.9238,
                size,
                theme.dim,
                tracking,
                Align::Center,
            );
        }
    }
}

fn load_font(path: &Path) -> Result<Font, String> {
    let bytes =
        fs::read(path).map_err(|err| format!("フォントを読めない {}: {err}", path.display()))?;
    Font::from_bytes(bytes, FontSettings::default())
        .map_err(|err| format!("フォントを解釈できない {}: {err}", path.display()))
}

#[derive(Debug, Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

fn fill(canvas: &mut [u8], color: Color) {
    for pixel in canvas.as_chunks_mut::<4>().0 {
        *pixel = [color.b, color.g, color.r, 0xff];
    }
}

fn fill_gradient(
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    top: Color,
    bottom: Color,
) {
    for y in 0..height {
        let t = if height <= 1 {
            0.0
        } else {
            y as f32 / (height - 1) as f32
        };
        let color = top.mix(bottom, t);
        let row = (y * stride) as usize;
        for x in 0..width {
            let index = row + x as usize * 4;
            canvas[index] = color.b;
            canvas[index + 1] = color.g;
            canvas[index + 2] = color.r;
            canvas[index + 3] = 0xff;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color,
) {
    let x0 = x.clamp(0, width);
    let y0 = y.clamp(0, height);
    let x1 = x.saturating_add(w).clamp(0, width);
    let y1 = y.saturating_add(h).clamp(0, height);
    for row in y0..y1 {
        for col in x0..x1 {
            put(canvas, stride, width, height, col, row, color);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_circle(
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    cx: f32,
    cy: f32,
    radius: f32,
    color: Color,
) {
    let radius = radius.max(0.0);
    let x0 = (cx - radius - 1.0).floor() as i32;
    let y0 = (cy - radius - 1.0).floor() as i32;
    let x1 = (cx + radius + 1.0).ceil() as i32;
    let y1 = (cy + radius + 1.0).ceil() as i32;
    for y in y0.max(0)..y1.min(height) {
        for x in x0.max(0)..x1.min(width) {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
            blend(canvas, stride, width, height, x, y, color, coverage);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rounded_rect(
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: Color,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let radius = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = (x + w).ceil() as i32;
    let y1 = (y + h).ceil() as i32;
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    let half_w = w / 2.0 - radius;
    let half_h = h / 2.0 - radius;
    for py in y0.max(0)..y1.min(height) {
        for px in x0.max(0)..x1.min(width) {
            let qx = (px as f32 + 0.5 - cx).abs() - half_w;
            let qy = (py as f32 + 0.5 - cy).abs() - half_h;
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let distance = outside + qx.max(qy).min(0.0) - radius;
            let coverage = (0.5 - distance).clamp(0.0, 1.0);
            blend(canvas, stride, width, height, px, py, color, coverage);
        }
    }
}

fn put(canvas: &mut [u8], stride: i32, width: i32, height: i32, x: i32, y: i32, color: Color) {
    if x < 0 || y < 0 || x >= width || y >= height {
        return;
    }
    let index = (y * stride + x * 4) as usize;
    if index + 3 >= canvas.len() {
        return;
    }
    canvas[index] = color.b;
    canvas[index + 1] = color.g;
    canvas[index + 2] = color.r;
    canvas[index + 3] = 0xff;
}

#[allow(clippy::too_many_arguments)]
fn blend(
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    color: Color,
    coverage: f32,
) {
    if x < 0 || y < 0 || x >= width || y >= height {
        return;
    }
    let coverage = coverage.clamp(0.0, 1.0);
    if coverage <= 0.0 {
        return;
    }
    let index = (y * stride + x * 4) as usize;
    if index + 3 >= canvas.len() {
        return;
    }
    if coverage >= 1.0 {
        canvas[index] = color.b;
        canvas[index + 1] = color.g;
        canvas[index + 2] = color.r;
    } else {
        let inv = 1.0 - coverage;
        canvas[index] = (color.b as f32 * coverage + canvas[index] as f32 * inv).round() as u8;
        canvas[index + 1] =
            (color.g as f32 * coverage + canvas[index + 1] as f32 * inv).round() as u8;
        canvas[index + 2] =
            (color.r as f32 * coverage + canvas[index + 2] as f32 * inv).round() as u8;
    }
    canvas[index + 3] = 0xff;
}

fn measure(font: &Font, text: &str, size: f32, tracking: f32) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let mut width: f32 = text
        .chars()
        .map(|c| font.metrics(c, size).advance_width)
        .sum();
    width += tracking * text.chars().count().saturating_sub(1) as f32;
    width
}

fn fit_size(font: &Font, text: &str, size: f32, max_width: f32, tracking: f32) -> f32 {
    let width = measure(font, text, size, tracking);
    if width <= max_width || width <= 0.0 {
        size
    } else {
        (size * max_width / width).max(8.0)
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    font: &Font,
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    text: &str,
    x: f32,
    center_y: f32,
    size: f32,
    color: Color,
    tracking: f32,
    align: Align,
) {
    if text.is_empty() {
        return;
    }

    let metrics: Vec<_> = text.chars().map(|c| font.metrics(c, size)).collect();
    let total: f32 = metrics.iter().map(|m| m.advance_width).sum::<f32>()
        + tracking * metrics.len().saturating_sub(1) as f32;
    let start = match align {
        Align::Left => x,
        Align::Center => x - total / 2.0,
        Align::Right => x - total,
    };
    let baseline = text_baseline(font, size, center_y, &metrics).round() as i32;
    let mut pen = start;
    for (c, metric) in text.chars().zip(metrics.iter()) {
        let (_, bitmap) = font.rasterize(c, size);
        let x = pen.round() as i32 + metric.xmin;
        let y = glyph_top(metric, baseline);
        blit(
            canvas,
            stride,
            width,
            height,
            &bitmap,
            metric.width,
            x,
            y,
            color,
        );
        pen += metric.advance_width + tracking;
    }
}

/// 行のベースライン（画面座標、下向き正）。
///
/// 文字ごとのビットマップを縦中央へ寄せると、小文字や降下部が大文字と
/// 同じ帯に浮いて見える。フォントの ascent / descent から行の中心を求め、
/// すべてのグリフを同じベースラインに載せる。
fn text_baseline(font: &Font, size: f32, center_y: f32, metrics: &[Metrics]) -> f32 {
    if let Some(line) = font.horizontal_line_metrics(size) {
        return center_y + (line.ascent + line.descent) / 2.0;
    }
    ink_baseline(center_y, metrics)
}

/// 行メトリクスを持たないフォント向けのフォールバック。この行のグリフの
/// 上端と下端から中心を求める。
fn ink_baseline(center_y: f32, metrics: &[Metrics]) -> f32 {
    if metrics.is_empty() {
        return center_y;
    }
    let ascent = metrics
        .iter()
        .map(|m| (m.ymin + m.height as i32) as f32)
        .fold(f32::MIN, f32::max);
    let descent = metrics
        .iter()
        .map(|m| m.ymin as f32)
        .fold(f32::MAX, f32::min);
    center_y + (ascent + descent) / 2.0
}

/// グリフのビットマップ上端の y。`ymin` はベースラインからビットマップ
/// 下端までの上向きオフセット（降下部は負）なので、上端はさらに
/// `height` だけ上になる。
fn glyph_top(metric: &Metrics, baseline: i32) -> i32 {
    baseline - metric.ymin - metric.height as i32
}

#[allow(clippy::too_many_arguments)]
fn blit(
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    bitmap: &[u8],
    glyph_width: usize,
    x: i32,
    y: i32,
    color: Color,
) {
    if glyph_width == 0 || bitmap.is_empty() {
        return;
    }
    for (row, line) in bitmap.chunks_exact(glyph_width).enumerate() {
        for (col, coverage) in line.iter().enumerate() {
            if *coverage == 0 {
                continue;
            }
            let px = x + col as i32;
            let py = y + row as i32;
            if px < 0 || py < 0 || px >= width || py >= height {
                continue;
            }
            let index = (py * stride + px * 4) as usize;
            if index + 3 >= canvas.len() {
                continue;
            }
            let alpha = *coverage as u32;
            for (offset, source) in [color.b, color.g, color.r].into_iter().enumerate() {
                let dst = canvas[index + offset] as u32;
                canvas[index + offset] =
                    ((source as u32 * alpha + dst * (255 - alpha)) / 255) as u8;
            }
            canvas[index + 3] = 0xff;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::from_config(&UiConfig::default()).unwrap()
    }

    fn view(blank: bool) -> View<'static> {
        View {
            mode: Mode::Pin,
            secret_len: 4,
            clock: "14:53",
            date: "Sunday, 15 November 2026",
            message: "",
            label: "THINKPAD",
            blank,
        }
    }

    fn pixel(canvas: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        let i = y * stride + x * 4;
        [canvas[i], canvas[i + 1], canvas[i + 2], canvas[i + 3]]
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(
            Color::from_hex("#1b2a6b"),
            Some(Color::rgb(0x1b, 0x2a, 0x6b))
        );
        assert_eq!(
            Color::from_hex("#AABBCC"),
            Some(Color::rgb(0xaa, 0xbb, 0xcc))
        );
        assert_eq!(Color::from_hex("1b2a6b"), None);
        assert_eq!(Color::from_hex("#12345"), None);
        assert_eq!(Color::from_hex("#gggggg"), None);
    }

    #[test]
    fn theme_rejects_invalid_color_from_config() {
        let ui = UiConfig {
            background: "blue".to_string(),
            ..UiConfig::default()
        };
        assert!(Theme::from_config(&ui).is_err());
    }

    #[test]
    fn default_theme_is_ink() {
        let theme = Theme::default();
        assert_eq!(theme.background, Color::rgb(0x15, 0x18, 0x1b));
        assert_eq!(theme.pin_accent, Color::rgb(0x8f, 0xc7, 0xb8));
    }

    #[test]
    fn blank_frame_is_pure_black() {
        let (width, height) = (64, 48);
        let stride = width * 4;
        let mut canvas = vec![0u8; stride * height];
        Renderer::fallback().render(
            &mut canvas,
            stride,
            width as u32,
            height as u32,
            &theme(),
            &view(true),
        );
        assert!(
            canvas
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [0, 0, 0, 0xff])
        );
    }

    #[test]
    fn background_gradient_runs_top_to_bottom() {
        let (width, height) = (64, 96);
        let stride = width * 4;
        let mut canvas = vec![0u8; stride * height];
        Renderer::fallback().render(
            &mut canvas,
            stride,
            width as u32,
            height as u32,
            &theme(),
            &view(false),
        );
        let top = pixel(&canvas, stride, 0, 10);
        let bottom = pixel(&canvas, stride, 0, height - 1);
        assert_ne!(top, bottom);
    }

    #[test]
    fn fallback_render_paints_shapes_without_fonts() {
        let (width, height) = (100, 100);
        let stride = width * 4;
        let mut canvas = vec![0u8; stride * height];
        let theme = theme();
        Renderer::fallback().render(
            &mut canvas,
            stride,
            width as u32,
            height as u32,
            &theme,
            &view(false),
        );
        let [b, g, r, a] = pixel(&canvas, stride, 50, 0);
        assert_eq!(
            [r, g, b, a],
            [
                theme.pin_accent.r,
                theme.pin_accent.g,
                theme.pin_accent.b,
                0xff
            ]
        );
    }

    #[test]
    fn rounded_rect_blends_corners() {
        let (width, height) = (20, 20);
        let stride = width * 4;
        let mut canvas = vec![0u8; stride * height];
        fill(&mut canvas, Color::rgb(0, 0, 0));
        fill_rounded_rect(
            &mut canvas,
            stride as i32,
            width as i32,
            height as i32,
            2.0,
            2.0,
            16.0,
            16.0,
            6.0,
            Color::rgb(255, 0, 0),
        );
        assert_eq!(pixel(&canvas, stride, 10, 10), [0, 0, 255, 0xff]);
        assert_eq!(pixel(&canvas, stride, 0, 0), [0, 0, 0, 0xff]);
    }

    #[test]
    fn circle_is_antialiased_at_edges() {
        let (width, height) = (20, 20);
        let stride = width * 4;
        let mut canvas = vec![0u8; stride * height];
        fill(&mut canvas, Color::rgb(0, 0, 0));
        fill_circle(
            &mut canvas,
            stride as i32,
            width as i32,
            height as i32,
            10.0,
            10.0,
            5.0,
            Color::rgb(255, 255, 255),
        );
        assert_eq!(pixel(&canvas, stride, 10, 10), [255, 255, 255, 0xff]);
        assert_eq!(pixel(&canvas, stride, 0, 0), [0, 0, 0, 0xff]);
    }

    #[test]
    fn hidpi_buffer_size_is_independent_of_layout() {
        for (width, height) in [(100, 100), (200, 200), (400, 300)] {
            let stride = width * 4;
            let mut canvas = vec![0u8; stride * height];
            Renderer::fallback().render(
                &mut canvas,
                stride,
                width as u32,
                height as u32,
                &theme(),
                &view(false),
            );
            assert!(
                canvas
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[3] == 0xff)
            );
        }
    }

    #[test]
    fn bottom_hint_is_uppercase() {
        assert_eq!(BOTTOM_HINT, BOTTOM_HINT.to_uppercase());
    }

    #[test]
    fn glyphs_sit_on_a_shared_baseline() {
        let baseline = 100;
        let cap = Metrics {
            ymin: 0,
            height: 14,
            ..Metrics::default()
        };
        let x_height = Metrics {
            ymin: 0,
            height: 9,
            ..Metrics::default()
        };
        let descender = Metrics {
            ymin: -3,
            height: 9,
            ..Metrics::default()
        };

        // 小文字を大文字と同じ上端に揃えない。下端（ベースライン）だけを揃える。
        assert_eq!(glyph_top(&cap, baseline), 86);
        assert_eq!(glyph_top(&x_height, baseline), 91);
        assert_eq!(
            glyph_top(&cap, baseline) + cap.height as i32,
            glyph_top(&x_height, baseline) + x_height.height as i32
        );
        // 降下部はベースラインより下へ出る。
        assert_eq!(glyph_top(&descender, baseline), 94);
        assert!(
            glyph_top(&descender, baseline) + descender.height as i32 > baseline,
            "descender must go below the baseline"
        );
    }

    #[test]
    fn ink_baseline_centers_glyph_extents() {
        let cap = Metrics {
            ymin: 0,
            height: 14,
            ..Metrics::default()
        };
        let descender = Metrics {
            ymin: -3,
            height: 9,
            ..Metrics::default()
        };
        // 上端 14、下端 -3 の帯を center_y のまわりに置く。
        assert_eq!(ink_baseline(50.0, &[cap, descender]), 55.5);
        assert_eq!(ink_baseline(50.0, &[]), 50.0);
    }
}
