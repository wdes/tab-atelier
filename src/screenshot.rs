// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use log::{debug, info};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

pub fn screenshot_dir() -> PathBuf {
    let pictures = std::process::Command::new("xdg-user-dir")
        .arg("PICTURES")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{home}/Pictures")
        });
    PathBuf::from(pictures).join("screenshots")
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

struct CapturedImage {
    width: u16,
    height: u16,
    data: Vec<u8>,
}

fn capture_focused_window() -> Result<CapturedImage, String> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("x11 connect: {e}"))?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let focus = conn.get_input_focus().map_err(|e| format!("get_input_focus: {e}"))?
        .reply().map_err(|e| format!("get_input_focus reply: {e}"))?;

    let mut window = focus.focus;
    loop {
        let tree = conn.query_tree(window).map_err(|e| format!("query_tree: {e}"))?
            .reply().map_err(|e| format!("query_tree reply: {e}"))?;
        if tree.parent == root || tree.parent == 0 {
            break;
        }
        window = tree.parent;
    }

    let geom = conn.get_geometry(window).map_err(|e| format!("get_geometry: {e}"))?
        .reply().map_err(|e| format!("get_geometry reply: {e}"))?;

    let coords = conn.translate_coordinates(window, root, 0, 0)
        .map_err(|e| format!("translate_coordinates: {e}"))?
        .reply().map_err(|e| format!("translate_coordinates reply: {e}"))?;

    debug!(
        "screenshot: capturing from root at ({},{}) size {}x{}",
        coords.dst_x, coords.dst_y, geom.width, geom.height
    );

    let reply = conn.get_image(
        ImageFormat::Z_PIXMAP,
        root,
        coords.dst_x, coords.dst_y,
        geom.width, geom.height,
        u32::MAX,
    ).map_err(|e| format!("get_image: {e}"))?
     .reply().map_err(|e| format!("get_image reply: {e}"))?;

    Ok(CapturedImage {
        width: geom.width,
        height: geom.height,
        data: reply.data,
    })
}

fn write_bmp(path: &std::path::Path, width: u16, height: u16, bgra: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let w = width as u32;
    let h = height as u32;
    let row_size = w * 3;
    let row_padded = (row_size + 3) & !3;
    let pixel_data_size = row_padded * h;
    let file_size = 54 + pixel_data_size;

    let mut f = std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;

    // BMP header
    f.write_all(b"BM").map_err(|e| e.to_string())?;
    f.write_all(&file_size.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&[0u8; 4]).map_err(|e| e.to_string())?; // reserved
    f.write_all(&54u32.to_le_bytes()).map_err(|e| e.to_string())?; // data offset

    // DIB header (BITMAPINFOHEADER)
    f.write_all(&40u32.to_le_bytes()).map_err(|e| e.to_string())?; // header size
    f.write_all(&w.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&h.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&1u16.to_le_bytes()).map_err(|e| e.to_string())?; // planes
    f.write_all(&24u16.to_le_bytes()).map_err(|e| e.to_string())?; // bits per pixel
    f.write_all(&[0u8; 24]).map_err(|e| e.to_string())?; // compression, sizes, resolution, colors

    let padding = vec![0u8; (row_padded - row_size) as usize];
    let bpp = if bgra.len() >= (w * h * 4) as usize { 4 } else { 3 };

    // BMP stores rows bottom-to-top
    for y in (0..h).rev() {
        for x in 0..w {
            let src = (y * w + x) as usize * bpp;
            if src + 2 < bgra.len() {
                // BGRA → BGR (BMP native order)
                f.write_all(&bgra[src..src + 3]).map_err(|e| e.to_string())?;
            } else {
                f.write_all(&[0, 0, 0]).map_err(|e| e.to_string())?;
            }
        }
        if !padding.is_empty() {
            f.write_all(&padding).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .to_lowercase()
}

pub fn take_screenshot_full(tab_name: &str) -> Result<PathBuf, String> {
    let img = capture_focused_window()?;
    let dir = screenshot_dir();
    let _ = std::fs::create_dir_all(&dir);
    let safe = sanitize_filename(tab_name);
    let path = dir.join(format!("tab-atelier-{safe}-{}.bmp", timestamp()));
    write_bmp(&path, img.width, img.height, &img.data)?;
    info!("screenshot saved: {}", path.display());
    Ok(path)
}

pub fn take_screenshot_tab(tab_name: &str, tab_bar_height: u16) -> Result<PathBuf, String> {
    let img = capture_focused_window()?;
    if img.height <= tab_bar_height {
        return Err("window too small to crop tab bar".into());
    }

    let crop_h = img.height - tab_bar_height;
    let bpp = if img.data.len() >= (img.width as usize * img.height as usize * 4) { 4 } else { 3 };
    let src_stride = img.width as usize * bpp;
    let start = tab_bar_height as usize * src_stride;
    let cropped = if start < img.data.len() {
        img.data[start..].to_vec()
    } else {
        return Err("crop offset exceeds image data".into());
    };

    let dir = screenshot_dir();
    let _ = std::fs::create_dir_all(&dir);
    let safe = sanitize_filename(tab_name);
    let path = dir.join(format!("tab-atelier-tab-{safe}-{}.bmp", timestamp()));
    write_bmp(&path, img.width, crop_h, &cropped)?;
    info!("tab screenshot saved: {}", path.display());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_dir_returns_path() {
        let dir = screenshot_dir();
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn timestamp_is_numeric() {
        let ts = timestamp();
        assert!(ts.chars().all(|c| c.is_ascii_digit()));
        assert!(ts.parse::<u64>().unwrap() > 1_700_000_000);
    }

    #[test]
    fn write_bmp_creates_valid_file() {
        let dir = std::env::temp_dir().join("ta-test-bmp");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.bmp");

        // 2x2 blue image (BGRA)
        let data = vec![
            0xFF, 0x00, 0x00, 0xFF, // blue
            0x00, 0xFF, 0x00, 0xFF, // green
            0x00, 0x00, 0xFF, 0xFF, // red
            0xFF, 0xFF, 0xFF, 0xFF, // white
        ];
        write_bmp(&path, 2, 2, &data).unwrap();

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(&contents[0..2], b"BM");
        assert!(contents.len() > 54);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_removes_special_chars() {
        assert_eq!(sanitize_filename("Terminal"), "terminal");
        assert_eq!(sanitize_filename("my tab/name"), "my_tab_name");
        assert_eq!(sanitize_filename("hello world!"), "hello_world_");
        assert_eq!(sanitize_filename("a-b_c"), "a-b_c");
    }
}
