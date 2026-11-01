// SPDX-License-Identifier: MPL-2.0

#[cfg(feature = "gui")]
pub struct CapturedImage {
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::*;
