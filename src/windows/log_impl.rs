use simplelog::{ConfigBuilder, LevelFilter, WriteLogger};
use std::{fs::{self, File}, path::Path};

// hachimi.log is always the current session; earlier sessions are kept in hachimi\logs.
const KEEP_LOGS: usize = 30;

pub fn init(filter_level: log::LevelFilter, file_logging: bool) {
    if file_logging {
        let game_dir = super::utils::get_game_dir();
        let path = game_dir.join("hachimi.log");
        keep_old_log(&path, &game_dir.join("hachimi").join("logs"));

        if let Ok(file) = File::create(path) {
            let config = ConfigBuilder::new()
                .set_target_level(LevelFilter::Error)
                .add_filter_ignore_str("sqlparser")
                .set_time_format_rfc3339()
                .build();

            if WriteLogger::init(filter_level, config, file).is_ok() {
                return;
            }
        }
    }

    if let Some(level) = filter_level.to_level() {
        windebug_logger::init_with_level(level).ok();
    }
}

// Moves the last session's log into `dir`, named by when it was last written,
// then removes all but the newest KEEP_LOGS there.
fn keep_old_log(log: &Path, dir: &Path) {
    let Ok(modified) = fs::metadata(log).and_then(|m| m.modified()) else { return };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let stamp = chrono::DateTime::<chrono::Local>::from(modified).format("%Y%m%d-%H%M%S");
    if fs::rename(log, dir.join(format!("hachimi_{stamp}.log"))).is_err() {
        return;
    }
    let mut names: Vec<String> = fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("hachimi_") && n.ends_with(".log"))
        .collect();
    for name in oldest_beyond(&mut names, KEEP_LOGS) {
        let _ = fs::remove_file(dir.join(name));
    }
}

// The names hold a sortable time stamp, so sorted order is oldest first.
fn oldest_beyond(names: &mut Vec<String>, keep: usize) -> Vec<String> {
    names.sort();
    let extra = names.len().saturating_sub(keep);
    names.drain(..extra).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_logs_pruned_oldest_first() {
        let mut names: Vec<String> = ["hachimi_20261029-120000.log", "hachimi_20261027-090000.log", "hachimi_20261028-230000.log"]
            .map(String::from).to_vec();
        assert_eq!(oldest_beyond(&mut names, 2), ["hachimi_20261027-090000.log"]);
        assert_eq!(names, ["hachimi_20261028-230000.log", "hachimi_20261029-120000.log"]);
        assert!(oldest_beyond(&mut names, 30).is_empty(), "under the limit: nothing removed");
    }
}
