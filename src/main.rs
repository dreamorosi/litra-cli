use fs2::FileExt;
use hidapi::{HidApi, HidDevice};
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const HELP: &str = "Usage: litra [on|off|toggle|status]
       litra status --cached

No command means toggle. Commands query the light's actual power state.
status --cached prints the last-known state without accessing the light.
State is saved in $XDG_CONFIG_HOME/litra/state.txt or ~/.config/litra/state.txt.";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Command {
    On,
    Off,
    Toggle,
    Status,
    CachedStatus,
    Help,
    Version,
}

fn parse_command(args: &[String]) -> Result<Command> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["toggle"] => Ok(Command::Toggle),
        ["on"] => Ok(Command::On),
        ["off"] => Ok(Command::Off),
        ["status"] => Ok(Command::Status),
        ["status", "--cached"] => Ok(Command::CachedStatus),
        ["help"] | ["--help"] | ["-h"] => Ok(Command::Help),
        ["--version"] | ["-V"] => Ok(Command::Version),
        _ => Err(format!("invalid arguments\n\n{HELP}").into()),
    }
}

fn state_directory() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("XDG_CONFIG_HOME must be an absolute path".into());
        }
        return Ok(path.join("litra"));
    }
    let home = env::var_os("HOME")
        .filter(|p| !p.is_empty())
        .ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/litra"))
}

fn state_text(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

fn parse_state(text: &str) -> Result<bool> {
    match text.trim() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(
            "invalid saved state, expected 'on' or 'off'; run litra status to refresh it".into(),
        ),
    }
}

fn save_state(directory: &Path, on: bool) -> Result<()> {
    // The CLI holds state.lock while writing. Rename prevents a partial state file.
    let temporary = directory.join(format!("state.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = File::create(&temporary)?;
        writeln!(file, "{}", state_text(on))?;
        file.sync_all()?;
        fs::rename(&temporary, directory.join("state.txt"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn report(command: u8, value: u8) -> [u8; 20] {
    let mut bytes = [0; 20];
    bytes[..5].copy_from_slice(&[0x11, 0xff, 0x04, command, value]);
    bytes
}

trait Light {
    fn is_on(&self) -> Result<bool>;
    fn set_power(&self, on: bool) -> Result<()>;
}

fn write_report(device: &HidDevice, bytes: &[u8; 20]) -> Result<()> {
    if device.write(bytes)? != bytes.len() {
        return Err("incomplete USB HID write".into());
    }
    Ok(())
}

fn response_state(bytes: &[u8]) -> Result<Option<bool>> {
    // Ignore unrelated reports, such as physical-button notifications.
    if bytes.len() < 4 || bytes[..4] != [0x11, 0xff, 0x04, 0x01] {
        return Ok(None);
    }
    match bytes.get(4) {
        Some(0) => Ok(Some(false)),
        Some(1) => Ok(Some(true)),
        _ => Err("invalid power-state response from the light".into()),
    }
}

impl Light for HidDevice {
    fn is_on(&self) -> Result<bool> {
        write_report(self, &report(0x01, 0))?;
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut bytes = [0; 64];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out reading the light's power state".into());
            }
            let count = self.read_timeout(&mut bytes, remaining.as_millis().max(1) as i32)?;
            if count == 0 {
                return Err("timed out reading the light's power state".into());
            }
            if let Some(on) = response_state(&bytes[..count])? {
                return Ok(on);
            }
        }
    }

    fn set_power(&self, on: bool) -> Result<()> {
        write_report(self, &report(0x1c, u8::from(on)))
    }
}

fn execute_command(light: &impl Light, command: Command) -> Result<bool> {
    let target = match command {
        Command::On => true,
        Command::Off => false,
        Command::Toggle => !light.is_on()?,
        Command::Status => return light.is_on(),
        _ => return Err("command does not access the light".into()),
    };
    light.set_power(target)?;
    let actual = light.is_on()?;
    if actual != target {
        return Err(format!(
            "light did not switch {}; saved state was not changed",
            state_text(target)
        )
        .into());
    }
    Ok(actual)
}

fn run() -> Result<()> {
    let command = parse_command(&env::args().skip(1).collect::<Vec<_>>())?;
    match command {
        Command::Help => {
            println!("{HELP}");
            return Ok(());
        }
        Command::Version => {
            println!("litra {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    let directory = state_directory()?;
    fs::create_dir_all(&directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("state.lock"))?;
    // Do not wait indefinitely if another invocation holds the device.
    lock.try_lock_exclusive().map_err(|e| {
        if e.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(e.kind(), "another litra command is running; try again")
        } else {
            e
        }
    })?;

    if command == Command::CachedStatus {
        let text = fs::read_to_string(directory.join("state.txt")).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot read last-known state: {e}; run litra status first"),
            )
        })?;
        println!(
            "{} (last-known, not checked against the light)",
            state_text(parse_state(&text)?)
        );
        return Ok(());
    }

    let api = HidApi::new()?;
    let devices: Vec<_> = api
        .device_list()
        .filter(|d| d.vendor_id() == 0x046d && d.product_id() == 0xc900 && d.usage_page() == 0xff43)
        .collect();
    let info = match devices.as_slice() {
        [] => return Err("no Logitech Litra Glow found; check its USB connection".into()),
        [info] => info,
        _ => {
            return Err(
                "multiple Litra Glow devices found; connect only the one you want to control"
                    .into(),
            )
        }
    };
    let device = info.open_device(&api)?;
    let on = execute_command(&device, command)?;
    save_state(&directory, on).map_err(|e| {
        format!(
            "light is {}, but saving its last-known state failed: {e}",
            state_text(on)
        )
    })?;
    println!("{}", state_text(on));
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("litra: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct FakeLight {
        on: Cell<bool>,
        ignore_writes: bool,
        fail_reads: bool,
        fail_writes: bool,
        writes: Cell<usize>,
    }

    impl FakeLight {
        fn new(on: bool) -> Self {
            Self {
                on: Cell::new(on),
                ignore_writes: false,
                fail_reads: false,
                fail_writes: false,
                writes: Cell::new(0),
            }
        }
    }

    impl Light for FakeLight {
        fn is_on(&self) -> Result<bool> {
            if self.fail_reads {
                return Err("read failed".into());
            }
            Ok(self.on.get())
        }

        fn set_power(&self, on: bool) -> Result<()> {
            if self.fail_writes {
                return Err("write failed".into());
            }
            self.writes.set(self.writes.get() + 1);
            if !self.ignore_writes {
                self.on.set(on);
            }
            Ok(())
        }
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command(&[]).unwrap(), Command::Toggle);
        for (arg, expected) in [
            ("on", Command::On),
            ("off", Command::Off),
            ("toggle", Command::Toggle),
            ("status", Command::Status),
            ("--help", Command::Help),
            ("--version", Command::Version),
        ] {
            assert_eq!(parse_command(&[arg.into()]).unwrap(), expected);
        }
        assert_eq!(
            parse_command(&["status".into(), "--cached".into()]).unwrap(),
            Command::CachedStatus
        );
        assert!(parse_command(&["invalid".into()]).is_err());
        assert!(parse_command(&["on".into(), "extra".into()]).is_err());
    }

    #[test]
    fn toggles_actual_state_in_both_directions() {
        let light = FakeLight::new(false);
        assert!(execute_command(&light, Command::Toggle).unwrap());
        // A physical button changed the state between invocations.
        light.on.set(false);
        assert!(execute_command(&light, Command::Toggle).unwrap());
        assert!(!execute_command(&light, Command::Toggle).unwrap());
    }

    #[test]
    fn explicit_power_commands_are_idempotent() {
        let light = FakeLight::new(false);
        for _ in 0..2 {
            assert!(execute_command(&light, Command::On).unwrap());
        }
        for _ in 0..2 {
            assert!(!execute_command(&light, Command::Off).unwrap());
        }
    }

    #[test]
    fn status_does_not_change_power() {
        let light = FakeLight::new(true);
        assert!(execute_command(&light, Command::Status).unwrap());
        assert_eq!(light.writes.get(), 0);
    }

    #[test]
    fn detects_unconfirmed_changes_and_io_errors() {
        let mut light = FakeLight::new(false);
        light.ignore_writes = true;
        assert!(execute_command(&light, Command::On).is_err());
        light.fail_reads = true;
        assert!(execute_command(&light, Command::Toggle).is_err());
        assert_eq!(light.writes.get(), 1);
        light.fail_reads = false;
        light.fail_writes = true;
        assert!(execute_command(&light, Command::On).is_err());
    }

    #[test]
    fn matches_existing_hid_protocol() {
        let bytes = report(0x1c, 1);
        assert_eq!(&bytes[..5], &[0x11, 0xff, 0x04, 0x1c, 1]);
        assert!(bytes[5..].iter().all(|b| *b == 0));
        assert_eq!(response_state(&report(0x01, 1)).unwrap(), Some(true));
        assert_eq!(response_state(&report(0x01, 0)).unwrap(), Some(false));
        assert_eq!(response_state(&bytes).unwrap(), None);
        assert_eq!(response_state(&[]).unwrap(), None);
        assert!(response_state(&report(0x01, 2)).is_err());
        assert!(response_state(&[0x11, 0xff, 0x04, 0x01]).is_err());
    }

    #[test]
    fn reads_legacy_state_files() {
        assert!(parse_state("on").unwrap());
        assert!(!parse_state("off\n").unwrap());
        assert!(parse_state("unknown").is_err());
    }

    #[test]
    fn saves_state_atomically() {
        let directory = env::temp_dir().join(format!("litra-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        save_state(&directory, true).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("state.txt")).unwrap(),
            "on\n"
        );
        save_state(&directory, false).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("state.txt")).unwrap(),
            "off\n"
        );
        fs::remove_file(directory.join("state.txt")).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
