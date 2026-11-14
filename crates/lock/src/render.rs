//! `wl_shm` の ARGB8888 バッファへ直接描くソフトウェアレンダラ。
//!
//! 描くのは次の 3 層だけ。GPU もシェーピングも使わない。
//!
//! 1. 外周のモード色フレーム（太さはモードで変える）
//! 2. 上部 20% のモード帯（モード名を大きく表示）
//! 3. 中央の時計、入力ドット、状態メッセージ
//!
//! 文字は既定で ASCII のみを想定する。CJK を出す場合は `[ui] font` を
//! CJK 対応フォントへ差し替える。

use std::fs;
use std::path::Path;

use fontdue::{Font, FontSettings};
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
}

/// 画面全体で使う色とフレーム幅。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub dim: Color,
    pub pin_accent: Color,
    pub pin_on_accent: Color,
    pub password_accent: Color,
    pub password_on_accent: Color,
    pub frame_width: u32,
}

impl Theme {
    pub fn from_config(ui: &UiConfig) -> Result<Self, String> {
        Ok(Self {
            background: parse("background", &ui.background)?,
            foreground: parse("foreground", &ui.foreground)?,
            dim: parse("dim", &ui.dim)?,
            pin_accent: parse("pin_accent", &ui.pin_accent)?,
            pin_on_accent: parse("pin_on_accent", &ui.pin_on_accent)?,
            password_accent: parse("password_accent", &ui.password_accent)?,
            password_on_accent: parse("password_on_accent", &ui.password_on_accent)?,
            frame_width: ui.frame_width,
        })
    }

    fn colors(self, mode: Mode) -> (Color, Color) {
        match mode {
            Mode::Pin => (self.pin_accent, self.pin_on_accent),
            Mode::Password => (self.password_accent, self.password_on_accent),
        }
    }
}

fn parse(name: &str, value: &str) -> Result<Color, String> {
    Color::from_hex(value)
        .ok_or_else(|| format!("[ui] {name} を #rrggbb として解釈できない: {value}"))
}

/// 外周フレームの太さ。色だけに頼らず、太さでもモードを区別できるようにする。
/// `frame_width = 0` なら枠を描かない。
pub fn frame_thickness(theme: &Theme, mode: Mode) -> u32 {
    if theme.frame_width == 0 {
        return 0;
    }
    match mode {
        Mode::Pin => theme.frame_width,
        Mode::Password => theme.frame_width.saturating_add(4),
    }
}

/// 上部のモード帯の高さ（画面の 20%）。
fn band_height(height: u32) -> u32 {
    (height * 20 / 100).clamp(1, height.max(1))
}

/// 1 フレーム分の表示内容。
pub struct View<'a> {
    pub mode: Mode,
    pub secret_len: usize,
    pub clock: &'a str,
    pub message: &'a str,
}

/// フォントを保持するレンダラ。
pub struct Renderer {
    font: Font,
}

impl Renderer {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path)
            .map_err(|err| format!("フォントを読めない {}: {err}", path.display()))?;
        let font = Font::from_bytes(bytes, FontSettings::default())
            .map_err(|err| format!("フォントを解釈できない {}: {err}", path.display()))?;
        Ok(Self { font })
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

        let (accent, on_accent) = theme.colors(view.mode);
        fill(canvas, theme.background);

        let band = band_height(height as u32) as i32;
        fill_rect(canvas, stride, width, height, 0, 0, width, band, accent);

        let frame = frame_thickness(theme, view.mode) as i32;
        if frame > 0 {
            paint_frame(canvas, stride, width, height, frame, accent);
        }

        let mode_size = (band as f32 * 0.45).clamp(12.0, 200.0);
        draw_centered(
            &self.font,
            canvas,
            stride,
            width,
            height,
            view.mode.label(),
            width / 2,
            band / 2,
            mode_size,
            on_accent,
        );

        let clock_size = ((height as f32 / 16.0).min(width as f32 / 18.0)).clamp(12.0, 96.0);
        draw_centered(
            &self.font,
            canvas,
            stride,
            width,
            height,
            view.clock,
            width / 2,
            height * 55 / 100,
            clock_size,
            theme.foreground,
        );

        draw_dots(
            canvas,
            stride,
            width,
            height,
            view.secret_len,
            height * 68 / 100,
            on_accent,
        );

        let message_size = ((height as f32 / 32.0).min(width as f32 / 48.0)).clamp(10.0, 40.0);
        draw_centered(
            &self.font,
            canvas,
            stride,
            width,
            height,
            view.message,
            width / 2,
            height * 82 / 100,
            message_size,
            theme.dim,
        );
    }
}

fn fill(canvas: &mut [u8], color: Color) {
    for pixel in canvas.as_chunks_mut::<4>().0 {
        *pixel = [color.b, color.g, color.r, 0xff];
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

fn paint_frame(
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    thickness: i32,
    color: Color,
) {
    fill_rect(canvas, stride, width, height, 0, 0, width, thickness, color);
    fill_rect(
        canvas,
        stride,
        width,
        height,
        0,
        height - thickness,
        width,
        thickness,
        color,
    );
    fill_rect(
        canvas, stride, width, height, 0, 0, thickness, height, color,
    );
    fill_rect(
        canvas,
        stride,
        width,
        height,
        width - thickness,
        0,
        thickness,
        height,
        color,
    );
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
fn draw_centered(
    font: &Font,
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    text: &str,
    center_x: i32,
    center_y: i32,
    size: f32,
    color: Color,
) {
    if text.is_empty() {
        return;
    }

    let metrics: Vec<_> = text.chars().map(|c| font.metrics(c, size)).collect();
    let total_width: i32 = metrics.iter().map(|m| m.advance_width.round() as i32).sum();
    let max_height = metrics.iter().map(|m| m.height).max().unwrap_or(0);

    let top = center_y - max_height as i32 / 2;
    let mut pen = center_x - total_width / 2;
    for (c, metric) in text.chars().zip(metrics.iter()) {
        let (_, bitmap) = font.rasterize(c, size);
        let x = pen + metric.xmin;
        let y = top + (max_height.saturating_sub(metric.height) / 2) as i32;
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
        pen += metric.advance_width.round() as i32;
    }
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

fn draw_dots(
    canvas: &mut [u8],
    stride: i32,
    width: i32,
    height: i32,
    secret_len: usize,
    center_y: i32,
    color: Color,
) {
    if secret_len == 0 {
        return;
    }
    let radius = (height / 120).clamp(3, 10);
    let gap = radius * 3;
    let max_dots = ((width * 4 / 5) / gap).max(1) as usize;
    let count = secret_len.min(max_dots) as i32;
    let total = count * gap - radius;
    let start_x = width / 2 - total / 2;
    for i in 0..count {
        let cx = start_x + i * gap + radius / 2;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx * dx + dy * dy <= radius * radius {
                    put(canvas, stride, width, height, cx + dx, center_y + dy, color);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::from_config(&UiConfig::default()).unwrap()
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
    fn frame_is_thicker_in_password_mode() {
        let theme = theme();
        assert_eq!(frame_thickness(&theme, Mode::Pin), 12);
        assert_eq!(frame_thickness(&theme, Mode::Password), 16);

        let theme = Theme {
            frame_width: 0,
            ..theme
        };
        assert_eq!(frame_thickness(&theme, Mode::Pin), 0);
        assert_eq!(frame_thickness(&theme, Mode::Password), 0);
    }

    #[test]
    fn band_is_top_fifth() {
        assert_eq!(band_height(720), 144);
        assert_eq!(band_height(1080), 216);
        assert_eq!(band_height(1), 1);
    }

    #[test]
    fn frame_and_band_are_painted() {
        let (width, height) = (100, 100);
        let stride = width * 4;
        let mut canvas = vec![0u8; stride * height];
        let theme = theme();
        fill(&mut canvas, theme.background);
        fill_rect(
            &mut canvas,
            stride as i32,
            width as i32,
            height as i32,
            0,
            0,
            width as i32,
            band_height(height as u32) as i32,
            theme.pin_accent,
        );
        paint_frame(
            &mut canvas,
            stride as i32,
            width as i32,
            height as i32,
            4,
            theme.pin_accent,
        );

        let [b, g, r, a] = pixel(&canvas, stride, 0, 0);
        assert_eq!(
            [r, g, b, a],
            [
                theme.pin_accent.r,
                theme.pin_accent.g,
                theme.pin_accent.b,
                0xff
            ]
        );
        // 帯の外側（フレームの内側）は背景。
        let [b, g, r, _] = pixel(&canvas, stride, 10, 30);
        assert_eq!(
            [r, g, b],
            [theme.background.r, theme.background.g, theme.background.b]
        );
        // 下端のフレーム。
        let [b, g, r, _] = pixel(&canvas, stride, 50, 99);
        assert_eq!(
            [r, g, b],
            [theme.pin_accent.r, theme.pin_accent.g, theme.pin_accent.b]
        );
    }

    #[test]
    fn fill_rect_clips_to_canvas() {
        let (width, height) = (8, 8);
        let stride = width * 4;
        let mut canvas = vec![0u8; stride * height];
        fill(&mut canvas, Color::rgb(0, 0, 0));
        fill_rect(
            &mut canvas,
            stride as i32,
            width as i32,
            height as i32,
            -2,
            -2,
            4,
            4,
            Color::rgb(255, 0, 0),
        );
        assert_eq!(pixel(&canvas, stride, 0, 0), [0, 0, 255, 0xff]);
        assert_eq!(pixel(&canvas, stride, 2, 2), [0, 0, 0, 0xff]);
    }
}
