//! The app icon, decoded from the PNGs in `assets/`.

pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn decode(bytes: &[u8]) -> Rgba {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = dec.read_info().expect("bundled icon is a valid PNG");
    let mut buf = vec![0; reader.output_buffer_size().expect("icon size")];
    let info = reader.next_frame(&mut buf).expect("bundled icon decodes");
    buf.truncate(info.buffer_size());
    Rgba {
        width: info.width,
        height: info.height,
        rgba: buf,
    }
}

/// 256x256 window/app icon.
pub fn app() -> Rgba {
    decode(include_bytes!("../assets/icon-256.png"))
}

/// 32x32 tray icon.
pub fn tray() -> Rgba {
    decode(include_bytes!("../assets/tray-32.png"))
}

/// macOS menu-bar icon: a black glyph on transparent, drawn by macOS as a template
/// (white or black to match the menu bar, like other menu-bar icons).
pub fn tray_template() -> Rgba {
    decode(include_bytes!("../assets/tray-template.png"))
}
