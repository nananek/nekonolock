//! README / docs 用のロック画面プレビューを PNG に書き出す。
//!
//! ```sh
//! cargo run -p nekonolock --example preview
//! ```
//!
//! 出力先は `docs/images/`。実際のレンダラを通すので、リリース前に UI の
//! 回帰を目で確認する用途にも使える。

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use nekonolock::input::Mode;
use nekonolock::render::{Renderer, Theme, View};

const WIDTH: u32 = 2194;
const HEIGHT: u32 = 1234;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/images");
    std::fs::create_dir_all(&out)?;

    let ui_font = Path::new("/usr/share/fonts/TTF/DejaVuSans.ttf");
    let ink_clock = Path::new("/usr/share/fonts/TTF/DejaVuSans-ExtraLight.ttf");
    let paper_clock = Path::new("/usr/share/fonts/TTF/DejaVuSerif.ttf");

    let ink = Renderer::load(ui_font, Some(ink_clock))?;
    let paper = Renderer::load(ui_font, Some(paper_clock))?;

    for (name, renderer, theme, mode, secret_len) in [
        ("ink-pin", &ink, Theme::default(), Mode::Pin, 4),
        ("ink-password", &ink, Theme::default(), Mode::Password, 8),
        ("paper-pin", &paper, Theme::paper(), Mode::Pin, 4),
        ("paper-password", &paper, Theme::paper(), Mode::Password, 8),
    ] {
        let path = write_preview(&out, name, renderer, &theme, mode, secret_len)?;
        println!("{}", path.display());
    }
    Ok(())
}

fn write_preview(
    out: &Path,
    name: &str,
    renderer: &Renderer,
    theme: &Theme,
    mode: Mode,
    secret_len: usize,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let stride = WIDTH as usize * 4;
    let mut canvas = vec![0u8; stride * HEIGHT as usize];
    let view = View {
        mode,
        secret_len,
        clock: "14:53",
        date: "Sunday, 15 November 2026",
        message: "",
        label: "THINKPAD",
        blank: false,
    };
    renderer.render(&mut canvas, stride, WIDTH, HEIGHT, theme, &view);

    // ARGB8888 (little endian の BGRA) を PNG の RGBA に直す。
    for pixel in canvas.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }

    let path = out.join(format!("lock-screen-{name}.png"));
    let file = File::create(&path)?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&canvas)?;
    Ok(path)
}
