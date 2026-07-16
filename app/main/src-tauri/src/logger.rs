use std::fs::File;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use fern::colors::{Color, ColoredLevelConfig};
use tauri::AppHandle;
use tauri::Manager;
use time::OffsetDateTime;

use crate::store::get_logging_level;

/// Roll the log file over at this size.
///
/// Big enough to hold a whole session at `trace` level, which is what makes a
/// log useful when diagnosing something, while still bounded so it can't fill
/// the disk. The previous 40 KB cap dated from issue #268 (log filling the
/// disk); the actual cause there — the mDNS discovery busy-spin — is fixed, and
/// at trace level that cap rotated the interesting part away within seconds.
const MAX_LOG_FILE_SIZE: u128 = 5 * 1024 * 1024;

pub fn set_up_logging(app_handle: &AppHandle) -> Result<(), anyhow::Error> {
    let default_level = match std::env::var("RQS_LOG") {
        Ok(r) => {
            println!("set_up_logging: level asked: {:?}", r);
            log::LevelFilter::from_str(&r).unwrap_or(log::LevelFilter::Debug)
        }
        Err(_) => match get_logging_level(app_handle) {
            Some(level_str) => {
                println!("set_up_logging: level from config: {:?}", level_str);
                log::LevelFilter::from_str(&level_str).unwrap_or(log::LevelFilter::Info)
            }
            None => {
                if cfg!(debug_assertions) {
                    log::LevelFilter::Trace
                } else {
                    log::LevelFilter::Info
                }
            }
        },
    };

    println!("set_up_logging: level: {:?}", default_level);
    let colors = ColoredLevelConfig::new()
        .error(Color::Red)
        .warn(Color::Yellow)
        .info(Color::Green)
        .debug(Color::Blue)
        .trace(Color::Cyan);

    let dispatch = fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "\x1B[2m{date}\x1b[0m {level: >5} \x1B[2m{target}:\x1b[0m {message}",
                date = humantime::format_rfc3339_seconds(SystemTime::now()),
                target = record.target(),
                level = colors.color(record.level()),
                message = message,
            ));
        })
        // Let fern itself pass everything, and gate on the *global* max level
        // (set below). That's the check the log macros make first, so it costs
        // nothing while filtered - and unlike fern's own filter it can be
        // changed at runtime, which is what lets the settings UI switch levels
        // without a restart.
        .level(log::LevelFilter::Trace)
        .level_for("mdns_sd", log::LevelFilter::Error)
        // mdns-sd polls its sockets with `mio` as of upstream 0.13.0, which
        // replaced `polling`. It logs a TRACE line per (re)registration, so at
        // Trace it drowns the log - hundreds of lines between anything useful.
        // The `polling` entry below is what used to cover this; it stayed while
        // the crate underneath was swapped out, so keep both until `polling` is
        // confirmed gone from the tree.
        .level_for("mio", log::LevelFilter::Error)
        .level_for("polling", log::LevelFilter::Error)
        .level_for("neli", log::LevelFilter::Error)
        .level_for("bluez_async", log::LevelFilter::Error)
        .level_for("bluer", log::LevelFilter::Error)
        .level_for("async_io", log::LevelFilter::Error)
        .level_for("btleplug", log::LevelFilter::Error)
        .chain(std::io::stdout());

    if let Ok(path) = app_handle.path().app_log_dir() {
        if !path.exists() {
            std::fs::create_dir_all(&path)?;
        }

        let app_name = &app_handle.package_info().name;
        let file_logger = fern::log_file(get_log_file_path(&path, app_name, MAX_LOG_FILE_SIZE)?)?;

        dispatch.chain(file_logger).apply()?;
    } else {
        dispatch.apply()?;
    }

    // `apply()` raised the global max to match the dispatch (Trace); drop it to
    // what was actually asked for. `change_logging_level` moves it later.
    log::set_max_level(default_level);

    debug!("Finished setting up logging! yay!");
    Ok(())
}

fn get_log_file_path(
    dir: &impl AsRef<Path>,
    file_name: &str,
    max_file_size: u128,
) -> Result<PathBuf, anyhow::Error> {
    let path = dir.as_ref().join(format!("{file_name}.log"));

    if path.exists() {
        let log_size = File::open(&path)?.metadata()?.len() as u128;
        if log_size > max_file_size {
            let to = dir.as_ref().join(format!(
                "{}_{}.log",
                file_name,
                OffsetDateTime::now_utc()
                    .format(
                        &time::format_description::parse_borrowed::<2>(
                            "[year]-[month]-[day]_[hour]-[minute]-[second]"
                        )
                        .unwrap()
                    )
                    .unwrap(),
            ));

            if to.is_file() {
                let mut to_bak = to.clone();
                to_bak.set_file_name(format!(
                    "{}.bak",
                    to_bak.file_name().unwrap().to_string_lossy()
                ));
                std::fs::rename(&to, to_bak)?;
            }

            std::fs::rename(&path, to)?;
        }
    }

    Ok(path)
}
