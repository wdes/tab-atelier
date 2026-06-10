// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "gui")]

use std::path::PathBuf;

use log::info;

use crate::platform;

pub fn screenshot_dir() -> PathBuf {
    platform::pictures_dir().join("screenshots")
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

fn write_bmp(path: &std::path::Path, width: u16, height: u16, bgra: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let w = width as u32;
    let h = height as u32;
    let row_size = w * 3;
    let row_padded = (row_size + 3) & !3;
    let pixel_data_size = row_padded * h;
    let file_size = 54 + pixel_data_size;

    // The screenshot path is predictable (`tab-atelier-<name>-<unix
    // secs>.bmp`), so a local attacker could pre-plant a symlink there
    // pointing at a victim-writable file and have the next capture
    // truncate it. Drop any pre-existing entry (incl. a symlink) and
    // create exclusively (`O_EXCL`), which refuses to create through a
    // symlink — so the write can't be redirected.
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;

    // BMP header
    f.write_all(b"BM").map_err(|e| e.to_string())?;
    f.write_all(&file_size.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&[0u8; 4]).map_err(|e| e.to_string())?;
    f.write_all(&54u32.to_le_bytes()).map_err(|e| e.to_string())?;

    // DIB header (BITMAPINFOHEADER)
    f.write_all(&40u32.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&w.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&h.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&1u16.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&24u16.to_le_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&[0u8; 24]).map_err(|e| e.to_string())?;

    let padding = vec![0u8; (row_padded - row_size) as usize];
    let bpp = if bgra.len() >= (w * h * 4) as usize { 4 } else { 3 };

    for y in (0..h).rev() {
        for x in 0..w {
            let src = (y * w + x) as usize * bpp;
            if src + 2 < bgra.len() {
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
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .to_lowercase()
}

pub fn take_screenshot_full(tab_name: &str) -> Result<PathBuf, String> {
    let img = platform::capture_focused_window()?;
    let dir = screenshot_dir();
    let _ = std::fs::create_dir_all(&dir);
    let safe = sanitize_filename(tab_name);
    let path = dir.join(format!("tab-atelier-{safe}-{}.bmp", timestamp()));
    write_bmp(&path, img.width, img.height, &img.data)?;
    info!("screenshot saved: {}", path.display());
    Ok(path)
}

pub fn take_screenshot_tab(tab_name: &str, tab_bar_height: u16) -> Result<PathBuf, String> {
    let img = platform::capture_focused_window()?;
    if img.height <= tab_bar_height {
        return Err("window too small to crop tab bar".into());
    }

    let crop_h = img.height - tab_bar_height;
    let bpp = if img.data.len() >= (img.width as usize * img.height as usize * 4) {
        4
    } else {
        3
    };
    let src_stride = img.width as usize * bpp;
    let end = crop_h as usize * src_stride;
    let cropped = if end <= img.data.len() {
        img.data[..end].to_vec()
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

        let data = vec![
            0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
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

    #[test]
    fn sanitize_empty_string() {
        assert_eq!(sanitize_filename(""), "");
    }

    #[test]
    fn sanitize_unicode() {
        assert_eq!(sanitize_filename("café"), "café");
        assert_eq!(sanitize_filename("tab 日本"), "tab_日本");
    }

    #[test]
    fn write_bmp_3byte_pixels() {
        let dir = std::env::temp_dir().join("ta-test-bmp-3byte");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test3.bmp");

        // 2x2 image with 3 bytes per pixel (no alpha)
        let data = vec![0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];
        write_bmp(&path, 2, 2, &data).unwrap();

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(&contents[0..2], b"BM");
        // 54 header + 2 rows * 8 bytes (6 pixel bytes + 2 padding each)
        assert_eq!(contents.len(), 54 + 16);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_bmp_1x1() {
        let dir = std::env::temp_dir().join("ta-test-bmp-1x1");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tiny.bmp");

        let data = vec![0xAA, 0xBB, 0xCC, 0xFF];
        write_bmp(&path, 1, 1, &data).unwrap();

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(&contents[0..2], b"BM");
        // 1 pixel = 3 bytes, padded to 4 bytes per row
        assert_eq!(contents.len(), 54 + 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_bmp_invalid_path() {
        let result = write_bmp(std::path::Path::new("/nonexistent/dir/file.bmp"), 1, 1, &[0; 4]);
        assert!(result.is_err());
    }
}
