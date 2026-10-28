use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, Url};

/// Files handed to the app to send: on the command line, by "Open With", or
/// dropped on its icon. Waits here until the front end takes them, so files
/// from a launch that lands while the window is still loading aren't lost.
#[derive(Default)]
pub struct LaunchFiles(Mutex<Vec<String>>);

impl LaunchFiles {
    pub fn new(files: Vec<String>) -> Self {
        Self(Mutex::new(files))
    }
}

/// The files among `args` (the arguments after the program name), resolved
/// against `cwd`. Takes plain paths and `file://` URIs, which some Linux file
/// managers pass instead; skips flags and anything that isn't an existing
/// file, since only files can be sent.
pub fn files_from_args(args: &[String], cwd: &Path) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let path = if arg.starts_with("file://") {
            match Url::parse(arg).ok().and_then(|url| url.to_file_path().ok()) {
                Some(path) => path,
                None => {
                    warn!("launch_files: not a local file URI: {arg}");
                    continue;
                }
            }
        } else {
            cwd.join(PathBuf::from(arg))
        };
        if !path.is_file() {
            warn!("launch_files: not a file, skipped: {}", path.display());
            continue;
        }
        let path = path.to_string_lossy().into_owned();
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files
}

/// Called with a later launch's arguments while this copy is running. Queues
/// its files and tells the front end to take them.
pub fn add_launch_files(app: &AppHandle, args: &[String], cwd: &Path) {
    let files = files_from_args(args, cwd);
    if files.is_empty() {
        return;
    }
    info!("launch_files: {} file(s) from another launch", files.len());
    if let Ok(mut queued) = app.state::<LaunchFiles>().0.lock() {
        queued.extend(files);
    }
    if let Err(e) = app.emit("launch_files", ()) {
        warn!("launch_files: couldn't tell the front end: {e}");
    }
}

/// The files waiting to be sent, emptying the queue.
#[tauri::command]
pub fn take_launch_files(state: tauri::State<'_, LaunchFiles>) -> Vec<String> {
    state
        .0
        .lock()
        .map(|mut files| std::mem::take(&mut *files))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory with one file in it, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rqs-launch-files-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("a b.txt"), b"x").unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                eprintln!("couldn't remove {}: {e}", self.0.display());
            }
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn relative_paths_resolve_against_the_launch_folder() {
        let dir = Scratch::new("relative");
        let files = files_from_args(&args(&["a b.txt"]), &dir.0);
        assert_eq!(files, vec![dir.0.join("a b.txt").to_string_lossy()]);
    }

    #[test]
    fn absolute_paths_and_file_uris_are_accepted() {
        let dir = Scratch::new("uri");
        let file = dir.0.join("a b.txt");
        let uri = Url::from_file_path(&file).unwrap().to_string();
        // The URI is percent-encoded, as file managers send it.
        assert!(uri.contains("a%20b.txt"));

        let elsewhere = std::env::temp_dir();
        let files = files_from_args(&args(&[file.to_str().unwrap(), &uri]), &elsewhere);
        // Both name the same file, so it's listed once.
        assert_eq!(files, vec![file.to_string_lossy()]);
    }

    #[test]
    fn flags_folders_and_missing_files_are_skipped() {
        let dir = Scratch::new("skip");
        let files = files_from_args(
            &args(&["--minimized", ".", "missing.txt", "https://example.com/x"]),
            &dir.0,
        );
        assert!(files.is_empty());
    }
}
