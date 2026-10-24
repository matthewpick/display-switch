//
// Copyright © 2020 Haim Gelfenbeyn
// This code is licensed under MIT license (see LICENSE.txt for details)
//
use crate::configuration::{Configuration, SwitchDirection};
use crate::input_source::InputSource;

use anyhow::{Error, Result};
use ddc_hi::{Ddc, Display, Handle};

use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::{thread, time};

/// VCP feature code for input select
const INPUT_SELECT: u8 = 0x60;
const RETRY_DELAY_MS: u64 = 3000;

/// How many times the input is written before giving up. Some displays acknowledge a write
/// and then ignore it: a Philips 279P1 connected over USB-C does this consistently.
const MAX_SWITCH_ATTEMPTS: u32 = 3;
/// How long to wait before each write of the input. Both `m1ddc` and MonitorControl's
/// `Arm64DDC` sleep this long before every write, and displays that ignore a switch tend to
/// be the ones sent a write with no delay in front of it.
const PRE_WRITE_DELAY: time::Duration = time::Duration::from_millis(10);
/// How many times the input is written per attempt. `m1ddc` writes twice unconditionally
/// (`DDC_ITERATIONS`), and MonitorControl defaults to two write cycles as well.
const WRITE_CYCLES: u32 = 2;
/// How long to wait after writing the input before reading it back. Measured on a Philips
/// 279P1: a read issued immediately after the write always fails with "invalid DDC/CI length",
/// while one issued about 60ms later succeeds.
const SETTLE_DELAY: time::Duration = time::Duration::from_millis(50);
/// How many times the input is read back before concluding that the write cannot be verified.
/// Reads on this hardware fail intermittently, so a single failure means very little.
const MAX_VERIFY_READS: u32 = 3;
/// How long to wait between those read-backs. A read itself takes about 50ms.
const VERIFY_READ_DELAY: time::Duration = time::Duration::from_millis(25);

/// Qualify a macOS display name with its EDID serial number, so that identical monitors
/// (which report the same `DisplayProductName`) can be told apart.
///
/// The serial is left out when it is absent, zero (which displays report when they have none),
/// or already part of the name, so that monitors naming themselves "LEN P27u-10 S/N 1144206897"
/// are not given the same number twice.
#[cfg(target_os = "macos")]
fn macos_display_id(id: &str, serial: Option<u32>) -> String {
    let serial = serial
        .filter(|serial| *serial != 0)
        .map(|serial| serial.to_string())
        .filter(|serial| !id.contains(serial.as_str()));
    std::iter::once(id)
        .chain(serial.as_deref())
        .collect::<Vec<&str>>()
        .join(" ")
}

fn display_name(display: &Display, index: Option<usize>) -> String {
    // Different OSes populate different fields of ddc-hi-rs info structure differently. Create
    // a synthetic "display_name" that makes sense on each OS
    #[cfg(target_os = "linux")]
    let display_id = vec![
        &display.info.manufacturer_id,
        &display.info.model_name,
        &display.info.serial_number,
    ]
    .into_iter()
    .flatten()
    .map(|s| s.as_str())
    .collect::<Vec<&str>>()
    .join(" ");
    #[cfg(target_os = "macos")]
    let display_id = macos_display_id(&display.info.id, crate::platform::display_serial(display));
    #[cfg(target_os = "windows")]
    let display_id = &display.info.id;

    if let Some(index) = index {
        format!("'{} #{}'", display_id, index)
    } else {
        format!("'{}'", display_id)
    }
}

fn are_display_names_unique(displays: &[Display]) -> bool {
    let mut hash = HashSet::new();
    displays.iter().all(|display| hash.insert(display_name(display, None)))
}

/// The display operations needed to switch an input, so that the retry logic below can be
/// exercised without a real display.
trait InputControl {
    fn get_input(&mut self) -> Result<u16>;
    fn set_input(&mut self, input: u16) -> Result<()>;
    fn wait(&mut self, delay: time::Duration);
}

struct DdcInputControl<'a> {
    handle: &'a mut Handle,
}

impl InputControl for DdcInputControl<'_> {
    fn get_input(&mut self) -> Result<u16> {
        Ok(self.handle.get_vcp_feature(INPUT_SELECT)?.value() & 0xff)
    }

    fn set_input(&mut self, input: u16) -> Result<()> {
        self.handle.set_vcp_feature(INPUT_SELECT, input)
    }

    fn wait(&mut self, delay: time::Duration) {
        thread::sleep(delay)
    }
}

/// What reading the input back after a write told us.
enum Verification {
    /// The display reports the requested input.
    Confirmed(time::Duration),
    /// The display acknowledged the write but still reports something else.
    Ignored(u16),
    /// The input could not be read back at all.
    Unknown,
}

/// Writes the input several times, as the displays that ignore a single write need, waiting
/// before each write.
fn write_input(control: &mut impl InputControl, input: InputSource) -> Result<()> {
    for _ in 0..WRITE_CYCLES {
        control.wait(PRE_WRITE_DELAY);
        control.set_input(input.value())?;
    }
    Ok(())
}

fn verify_input(control: &mut impl InputControl, display_name: &str, input: InputSource) -> Verification {
    let started = time::Instant::now();
    control.wait(SETTLE_DELAY);
    for read in 1..=MAX_VERIFY_READS {
        match control.get_input() {
            Ok(current) if current == input.value() => return Verification::Confirmed(started.elapsed()),
            Ok(current) => return Verification::Ignored(current),
            Err(err) => debug!(
                "Failed to read back the input of display {} ({:?}), read {} of {}",
                display_name, err, read, MAX_VERIFY_READS
            ),
        }
        if read < MAX_VERIFY_READS {
            control.wait(VERIFY_READ_DELAY);
        }
    }
    Verification::Unknown
}

fn switch_input(control: &mut impl InputControl, display_name: &str, input: InputSource) {
    match control.get_input() {
        Ok(current) if current == input.value() => {
            info!("Display {} is already set to {}", display_name, input);
            return;
        }
        Ok(_) => (),
        Err(err) => warn!("Failed to get current input for display {}: {:?}", display_name, err),
    }

    for attempt in 1..=MAX_SWITCH_ATTEMPTS {
        debug!(
            "Setting display {} to {} (attempt {} of {})",
            display_name, input, attempt, MAX_SWITCH_ATTEMPTS
        );
        if let Err(err) = write_input(control, input) {
            error!("Failed to set display {} to {} ({:?})", display_name, input, err);
            return;
        }
        match verify_input(control, display_name, input) {
            Verification::Confirmed(elapsed) => {
                debug!("Display {} reported {} after {:?}", display_name, input, elapsed);
                if attempt == 1 {
                    info!("Display {} set to {}", display_name, input);
                } else {
                    info!("Display {} set to {} on attempt {}", display_name, input, attempt);
                }
                return;
            }
            Verification::Ignored(current) => warn!(
                "Display {} acknowledged being set to {} but still reports {:#x} (attempt {} of {})",
                display_name, input, current, attempt, MAX_SWITCH_ATTEMPTS
            ),
            Verification::Unknown => {
                // The input cannot be read back, which also happens when the switch worked and
                // the display is already dropping off. The input has already been written
                // `WRITE_CYCLES` times, which is what m1ddc does unconditionally, so stop here
                // rather than retrying blind.
                info!("Display {} set to {}, could not verify", display_name, input);
                return;
            }
        }
    }
    error!(
        "Display {} did not accept being set to {} after {} attempts",
        display_name, input, MAX_SWITCH_ATTEMPTS
    );
}

fn try_switch_display(handle: &mut Handle, display_name: &str, input: InputSource) {
    switch_input(&mut DdcInputControl { handle }, display_name, input)
}

fn displays() -> Vec<Display> {
    let displays = Display::enumerate();
    if !displays.is_empty() {
        return displays;
    }

    // Under some conditions, such as when using a KVM, it's possible for the USB connection/disconnection events to
    // occur before the display(s) become available. We retry once after a bit of a delay in order to be more
    // forgiving with regard to timing.
    let delay_duration = time::Duration::from_millis(RETRY_DELAY_MS);
    warn!(
        "Did not detect any DDC-compatible displays. Retrying after {} second(s)...",
        delay_duration.as_secs()
    );
    thread::sleep(delay_duration);
    Display::enumerate()
}

pub fn log_current_source() {
    let displays = displays();
    if displays.is_empty() {
        error!("Did not detect any DDC-compatible displays!");
        return;
    }
    let unique_names = are_display_names_unique(&displays);
    for (index, mut display) in displays.into_iter().enumerate() {
        let display_name = display_name(&display, if unique_names { None } else { Some(index + 1) });
        match display.handle.get_vcp_feature(INPUT_SELECT) {
            Ok(raw_source) => {
                let source = InputSource::from(raw_source.value());
                info!("Display {} is currently set to {}", display_name, source);
            }
            Err(err) => {
                error!("Failed to get current input for display {}: {:?}", display_name, err);
            }
        }
    }
}

pub fn switch(config: &Configuration, switch_direction: SwitchDirection) {
    let displays = displays();
    if displays.is_empty() {
        error!("Did not detect any DDC-compatible displays!");
        return;
    }
    let unique_names = are_display_names_unique(&displays);
    for (index, mut display) in displays.into_iter().enumerate() {
        let display_name = display_name(&display, if unique_names { None } else { Some(index + 1) });
        let input_sources = config.configuration_for_monitor(&display_name);
        debug!("Input sources found for display {}: {:?}", display_name, input_sources);
        if let Some(input) = input_sources.source(switch_direction) {
            try_switch_display(&mut display.handle, &display_name, input);
        } else {
            info!(
                "Display {} is not configured to switch on USB {}",
                display_name, switch_direction
            );
        }
        if let Some(execute_command) = input_sources.execute_command(switch_direction) {
            run_command(execute_command)
        }
    }
    if let Some(execute_command) = config.default_input_sources.execute_command(switch_direction) {
        run_command(execute_command)
    }
}

fn run_command(execute_command: &str) {
    fn try_run_command(execute_command: &str) -> Result<()> {
        let mut arguments = shell_words::split(execute_command)?;
        if arguments.is_empty() {
            return Ok(());
        }

        let executable = arguments.remove(0);
        let output = Command::new(executable).args(arguments).stdin(Stdio::null()).output()?;
        let stdout = if !output.stdout.is_empty() {
            if let Ok(s) = String::from_utf8(output.stdout) {
                format!("Stdout = [{}]\n", s)
            } else {
                "Stdout was not UTF-8".to_string()
            }
        } else {
            "No stdout\n".to_string()
        };
        let stderr = if !output.stderr.is_empty() {
            if let Ok(s) = String::from_utf8(output.stderr) {
                format!("Stderr = [{}]\n", s)
            } else {
                "Stderr was not UTF-8".to_string()
            }
        } else {
            "No stderr\n".to_string()
        };

        if output.status.success() {
            debug!("External command output: {} {}", stdout, stderr);
            info!("External command '{}' executed successfully", execute_command);
            Ok(())
        } else {
            let msg = if let Some(code) = output.status.code() {
                format!("Exited with status {}\n", code)
            } else {
                "Exited because of a signal\n".to_string()
            };
            Err(Error::msg(format!("{} {} {}", msg, stdout, stderr)))
        }
    }

    try_run_command(execute_command)
        .unwrap_or_else(|err| error!("Error executing external command '{}': {}", execute_command, err))
}

#[cfg(all(test, target_os = "macos"))]
mod naming_tests {
    use super::macos_display_id;

    #[test]
    fn identical_monitors_are_told_apart_by_serial() {
        assert_eq!(macos_display_id("PHL 279P1", Some(2667)), "PHL 279P1 2667");
        assert_eq!(macos_display_id("PHL 279P1", Some(1126)), "PHL 279P1 1126");
    }

    #[test]
    fn serial_already_in_the_name_is_not_repeated() {
        assert_eq!(
            macos_display_id("LEN P27u-10 S/N 1144206897", Some(1144206897)),
            "LEN P27u-10 S/N 1144206897"
        );
    }

    #[test]
    fn missing_serial_leaves_the_name_alone() {
        assert_eq!(macos_display_id("PHL 279P1", None), "PHL 279P1");
        assert_eq!(macos_display_id("PHL 279P1", Some(0)), "PHL 279P1");
    }
}

#[cfg(test)]
mod switch_tests {
    use super::*;
    use std::collections::VecDeque;

    const TARGET: u16 = 0x11;
    const OTHER: u16 = 0x10;

    /// A display whose read-backs are scripted: `Some(value)` is reported, `None` fails.
    /// Once the script runs out, `settled` is reported instead.
    struct FakeDisplay {
        reads: VecDeque<Option<u16>>,
        settled: Option<u16>,
        writes: Vec<u16>,
        /// Everything the logic asked of the display, in order.
        events: Vec<String>,
    }

    impl FakeDisplay {
        fn new(reads: Vec<Option<u16>>, settled: Option<u16>) -> Self {
            Self {
                reads: reads.into(),
                settled,
                writes: Vec::new(),
                events: Vec::new(),
            }
        }
    }

    impl InputControl for FakeDisplay {
        fn get_input(&mut self) -> Result<u16> {
            let value = self.reads.pop_front().unwrap_or(self.settled);
            self.events.push(match value {
                Some(value) => format!("read {:#04x}", value),
                None => "read err".to_string(),
            });
            match value {
                Some(value) => Ok(value),
                None => Err(Error::msg("invalid DDC/CI length")),
            }
        }

        fn set_input(&mut self, input: u16) -> Result<()> {
            self.writes.push(input);
            self.events.push(format!("write {:#04x}", input));
            Ok(())
        }

        fn wait(&mut self, delay: time::Duration) {
            self.events.push(format!("wait {}ms", delay.as_millis()));
        }
    }

    fn switch(display: &mut FakeDisplay) {
        switch_input(display, "'Fake Display'", InputSource::Raw(TARGET))
    }

    #[test]
    fn display_already_on_the_input_is_left_alone() {
        let mut display = FakeDisplay::new(vec![Some(TARGET)], Some(TARGET));
        switch(&mut display);
        assert!(display.writes.is_empty(), "should not write to a display already set");
    }

    #[test]
    fn write_verified_on_the_first_attempt() {
        let mut display = FakeDisplay::new(vec![Some(OTHER), Some(TARGET)], Some(TARGET));
        switch(&mut display);
        assert_eq!(display.writes.len(), WRITE_CYCLES as usize);
    }

    #[test]
    fn every_write_is_preceded_by_a_wait() {
        let mut display = FakeDisplay::new(vec![Some(OTHER), Some(TARGET)], Some(TARGET));
        switch(&mut display);
        assert_eq!(
            display.events,
            vec![
                format!("read {:#04x}", OTHER),
                "wait 10ms".to_string(),
                format!("write {:#04x}", TARGET),
                "wait 10ms".to_string(),
                format!("write {:#04x}", TARGET),
                "wait 50ms".to_string(),
                format!("read {:#04x}", TARGET),
            ],
            "should write twice, waiting before each, then settle before reading back"
        );
    }

    #[test]
    fn write_ignored_once_is_retried_then_verified() {
        // Current input, then the display still reporting the old input, then accepting it.
        let mut display = FakeDisplay::new(vec![Some(OTHER), Some(OTHER), Some(TARGET)], Some(TARGET));
        switch(&mut display);
        assert_eq!(
            display.writes.len(),
            2 * WRITE_CYCLES as usize,
            "should make a second attempt"
        );
    }

    #[test]
    fn transient_read_failure_still_verifies() {
        // A read issued right after the write fails on real hardware; the next one succeeds.
        let mut display = FakeDisplay::new(vec![Some(OTHER), None, Some(TARGET)], Some(TARGET));
        switch(&mut display);
        assert_eq!(
            display.writes.len(),
            WRITE_CYCLES as usize,
            "a failed read should not trigger another attempt"
        );
    }

    #[test]
    fn unreadable_display_is_written_but_not_retried() {
        let mut display = FakeDisplay::new(vec![Some(OTHER)], None);
        switch(&mut display);
        assert_eq!(
            display.writes.len(),
            WRITE_CYCLES as usize,
            "should stop after one attempt rather than loop on read errors"
        );
    }

    #[test]
    fn display_that_keeps_ignoring_the_write_gives_up() {
        let mut display = FakeDisplay::new(vec![Some(OTHER)], Some(OTHER));
        switch(&mut display);
        assert_eq!(display.writes.len(), (MAX_SWITCH_ATTEMPTS * WRITE_CYCLES) as usize);
    }
}
