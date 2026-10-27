//! In-app updates through `tauri-plugin-updater`.
//!
//! The Release workflow publishes a signed `latest.json` with every release,
//! and the front end asks the plugin to check it. Only release builds carry the
//! public key the downloads are verified against; see `release.yml`.

use semver::Version;
use tauri::{AppHandle, Runtime};

/// The updater plugin, ordering versions with [`is_newer`].
pub fn plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R, tauri_plugin_updater::Config> {
    tauri_plugin_updater::Builder::new()
        .default_version_comparator(|current, remote| is_newer(&remote.version, &current))
        .build()
}

/// Whether `candidate` is a later release than `current`.
///
/// A release is 1.26.298+42: major, year, day of the year, and the Release
/// workflow's run number as build metadata, because Cargo only takes three
/// numbers. Semver ignores build metadata when ordering, so the plugin's own
/// comparison would never offer a second release made on the same day. Here
/// the run number counts as a fourth number, as it does everywhere the version
/// is shown.
pub fn is_newer(candidate: &Version, current: &Version) -> bool {
    release_order(candidate) > release_order(current)
}

fn release_order(version: &Version) -> (u64, u64, u64, u64) {
    // A build without a run number (a dev build's <major>.0.0) sorts before
    // every release of that day.
    let build = version.build.as_str().parse().unwrap_or(0);
    (version.major, version.minor, version.patch, build)
}

/// Restarts into the version the updater just installed.
///
/// Windows doesn't need this: the plugin hands over to the installer, which
/// closes the app and starts the new one. On Linux and macOS the files are
/// replaced in place and the running process is still the old version.
#[tauri::command]
pub fn restart_app<R: Runtime>(app: AppHandle<R>) {
    // Goes through ExitRequested, so the RQS service is stopped as on any
    // other exit before the new process starts.
    app.request_restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn a_later_run_on_the_same_day_is_newer() {
        assert!(is_newer(&v("1.26.298+43"), &v("1.26.298+42")));
        // Numbers, not strings: 10 is after 9.
        assert!(is_newer(&v("1.26.298+10"), &v("1.26.298+9")));
    }

    #[test]
    fn a_later_day_or_major_is_newer() {
        assert!(is_newer(&v("1.26.299+1"), &v("1.26.298+42")));
        assert!(is_newer(&v("1.27.1+1"), &v("1.26.365+99")));
        assert!(is_newer(&v("2.26.1+1"), &v("1.26.298+42")));
    }

    #[test]
    fn the_same_or_an_older_release_is_not_newer() {
        assert!(!is_newer(&v("1.26.298+42"), &v("1.26.298+42")));
        assert!(!is_newer(&v("1.26.298+41"), &v("1.26.298+42")));
        assert!(!is_newer(&v("1.26.297+99"), &v("1.26.298+42")));
    }
}
