//
// Copyright © 2020 Haim Gelfenbeyn
// This code is licensed under MIT license (see LICENSE.txt for details)
//

#[cfg(target_os = "macos")]
mod pnp_detect_libusb;
#[cfg(target_os = "macos")]
pub use pnp_detect_libusb::PnPDetectLibusb as PnPDetect;

#[cfg(target_os = "linux")]
mod pnp_detect_libusb;
#[cfg(target_os = "linux")]
pub use pnp_detect_libusb::PnPDetectLibusb as PnPDetect;

#[cfg(target_os = "windows")]
mod pnp_detect_windows;
#[cfg(target_os = "windows")]
pub use pnp_detect_windows::PnPDetectWindows as PnPDetect;

#[cfg(target_os = "macos")]
mod display_serial;
#[cfg(target_os = "macos")]
pub use display_serial::display_serial;

mod wake_displays;
pub use wake_displays::wake_displays;
