use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use notify::event::ModifyKind;
use serde::{Deserialize, Serialize};

// ============================================================
// CLI
// ============================================================
#[derive(Parser)]
#[command(name = "webox", about = "微信文件共享助手")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 一次性复制文件到微信 Shared 目录
    Send {
        /// 要发送的文件路径
        paths: Vec<PathBuf>,
    },
    /// 监听目录，新文件自动复制到微信 Shared
    Watch {
        /// 要监听的目录
        dir: PathBuf,
    },
    /// 列出已共享的文件
    List,
    /// 清理 Shared 中的文件（默认只清理 webox 管理的）
    Clean {
        /// 清理 Shared 目录里的所有文件
        #[arg(long)]
        all: bool,
    },
}

// ============================================================
// State
// ============================================================
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    files: HashMap<String, FileEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileEntry {
    original_name: String,
    copied_at: DateTime<Local>,
    source: PathBuf,
}

fn state_path() -> PathBuf {
    config_dir().join("webox").join("state.json")
}

fn shared_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("WEBOX_SHARED_DIR") {
        return PathBuf::from(dir);
    }
    data_dir().join("WeChat_Data").join("Shared")
}

fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".config"))
}

fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".local").join("share"))
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn load_state() -> State {
    let path = state_path();
    if path.exists() {
        match fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(state) => state,
                Err(_) => {
                    eprintln!("⚠ state.json 已损坏，已重置为空状态");
                    State::default()
                }
            },
            Err(_) => State::default(),
        }
    } else {
        State::default()
    }
}

fn save_state(state: &State) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        fs::write(&path, json).ok();
    }
}

// ============================================================
// Operations
// ============================================================

fn copy_to_shared(source: &Path, state: &mut State) -> anyhow::Result<PathBuf> {
    if !source.exists() {
        anyhow::bail!("文件不存在: {}", source.display());
    }
    if !source.is_file() {
        anyhow::bail!("不是文件: {}", source.display());
    }

    // 去重：如果该源文件已复制过，直接返回已有路径
    for (name, entry) in &state.files {
        if entry.source == source {
            let dest = shared_dir().join(name);
            if dest.exists() {
                println!("✓ 已存在: {}", dest.display());
                return Ok(dest);
            }
        }
    }

    let shared = shared_dir();
    fs::create_dir_all(&shared)?;

    let original_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // 处理重名
    let mut dest = shared.join(original_name);
    let mut counter = 1;
    while dest.exists() {
        let stem = Path::new(original_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file");
        let ext = Path::new(original_name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e))
            .unwrap_or_default();
        dest = shared.join(format!("{}_{}{}", stem, counter, ext));
        counter += 1;
    }

    fs::copy(source, &dest)?;

    let key = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    println!("✓ 已共享: {}", dest.display());

    state.files.insert(
        key,
        FileEntry {
            original_name: original_name.to_string(),
            copied_at: Local::now(),
            source: source.to_path_buf(),
        },
    );

    Ok(dest)
}

fn cmd_send(paths: &[PathBuf]) -> anyhow::Result<()> {
    let mut state = load_state();
    let total = paths.len();
    let mut success = 0usize;
    for path in paths {
        match copy_to_shared(path, &mut state) {
            Ok(_) => success += 1,
            Err(e) => eprintln!("✗ {}: {}", path.display(), e),
        }
    }
    save_state(&state);
    let failed = total - success;
    println!("已完成: {}, 失败: {}", success, failed);
    Ok(())
}

/// Returns true if an event kind should trigger file processing.
/// Filters out metadata-only modifications.
fn event_should_process(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) => true,
        EventKind::Modify(ModifyKind::Data(_)) => true,
        _ => false,
    }
}

fn should_skip(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|name| {
            name.starts_with('.')      // 隐藏文件
            || name.ends_with(".part")  // 部分下载
            || name.ends_with(".tmp")   // 临时文件
            || name.ends_with(".crdownload") // Chrome 下载中
        })
        .unwrap_or(true)
}

fn wait_file_ready(path: &Path, max_wait: Duration) -> bool {
    let start = std::time::Instant::now();
    let mut last_size = None;
    while start.elapsed() < max_wait {
        match fs::metadata(path) {
            Ok(meta) => {
                let size = meta.len();
                if Some(size) == last_size {
                    return true;
                }
                last_size = Some(size);
            }
            Err(_) => return false,
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn cleanup_processed(processed: &mut HashMap<PathBuf, std::time::Instant>, max_age: Duration) {
    let cutoff = std::time::Instant::now() - max_age;
    processed.retain(|_, time| *time > cutoff);
}

fn process_file(
    path: &Path,
    state: &mut State,
    processed: &mut HashMap<PathBuf, std::time::Instant>,
    debounce: Duration,
) {
    if !path.is_file() || should_skip(path) {
        return;
    }
    let canonical = match path.canonicalize() {
        Ok(c) => c,
        Err(_) => return,
    };
    let now = std::time::Instant::now();

    if let Some(last) = processed.get(&canonical) {
        if now.duration_since(*last) < debounce {
            return;
        }
    }

    if !wait_file_ready(path, Duration::from_millis(2000)) {
        eprintln!("✗ {}: 文件未就绪", path.display());
        return;
    }

    match copy_to_shared(path, state) {
        Ok(_) => {
            cleanup_processed(processed, Duration::from_secs(300));
            processed.insert(canonical, now);
            save_state(state);
        }
        Err(e) => {
            eprintln!("✗ {}: {}", path.display(), e);
        }
    }
}

fn cmd_watch(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        println!("已创建监听目录: {}", dir.display());
    }
    if !dir.is_dir() {
        anyhow::bail!("不是目录: {}", dir.display());
    }

    let shared = shared_dir();
    fs::create_dir_all(&shared)?;

    println!("👁  监听中: {} → {}", dir.display(), shared.display());
    println!("   拖文件进来就会自动共享。Ctrl+C 停止。");

    let (tx, rx) = mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            tx.send(event).ok();
        }
    })?;

    watcher.watch(dir, RecursiveMode::NonRecursive)?;

    let mut state = load_state();

    // 去重：记录已处理的文件（规范化路径）
    let mut processed: HashMap<PathBuf, std::time::Instant> = HashMap::new();
    let debounce = Duration::from_millis(500);

    // 尝试处理一个文件路径
    let try_process = |path: &Path, state: &mut State, processed: &mut HashMap<PathBuf, std::time::Instant>| {
        process_file(path, state, processed, debounce)
    };

    // 先处理已有文件
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && !should_skip(&path) {
                try_process(&path, &mut state, &mut processed);
            }
        }
    }

    // 持续监听
    loop {
        match rx.recv() {
            Ok(event) => match event.kind {
                kind if event_should_process(&kind) => {
                    for path in &event.paths {
                        if path.exists() {
                            try_process(path, &mut state, &mut processed);
                        }
                    }
                }
                _ => {}
            },
            Err(_) => break Ok(()),
        }
    }
}

fn cmd_list() -> anyhow::Result<()> {
    let state = load_state();
    if state.files.is_empty() {
        println!("暂无共享文件");
        return Ok(());
    }

    println!("已共享文件 ({} 个):", state.files.len());
    println!("{:<40} {:<20} {:<}", "文件名", "时间", "来源");
    println!("{}", "-".repeat(80));

    let mut entries: Vec<_> = state.files.iter().collect();
    entries.sort_by(|a, b| b.1.copied_at.cmp(&a.1.copied_at));

    for (name, entry) in entries {
        println!(
            "{:<40} {:<20} {:<}",
            name,
            entry.copied_at.format("%Y-%m-%d %H:%M"),
            entry.source.display()
        );
    }
    Ok(())
}

fn cmd_clean(all: bool) -> anyhow::Result<()> {
    let mut state = load_state();
    let shared = shared_dir();

    if all {
        let count = state.files.len();
        let mut removed = 0;
        for name in state.files.keys() {
            let path = shared.join(name);
            if path.exists() {
                fs::remove_file(&path)?;
                removed += 1;
            }
        }
        state.files.clear();
        save_state(&state);
        println!("✓ 已清理 {} 个文件（追踪 {} 个，实际删除 {} 个）", count, count, removed);
    } else {
        if state.files.is_empty() {
            println!("没有需要清理的文件");
            return Ok(());
        }
        let mut removed = 0;
        for name in state.files.keys() {
            let path = shared.join(name);
            if path.exists() {
                fs::remove_file(&path)?;
                removed += 1;
            }
        }
        let count = state.files.len();
        state.files.clear();
        save_state(&state);
        println!("✓ 已清理 {} 个文件（实际删除 {} 个）", count, removed);
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Send { paths } => {
            if paths.is_empty() {
                anyhow::bail!("请指定要发送的文件路径");
            }
            cmd_send(&paths)
        }
        Command::Watch { dir } => cmd_watch(&dir),
        Command::List => cmd_list(),
        Command::Clean { all } => cmd_clean(all),
    }
}

// ============================================================
// Tests
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::Mutex;
    use notify::event::{DataChange, MetadataKind, CreateKind};

    #[test]
    fn test_process_file_skips_when_cannot_canonicalize() {
        with_test_env(|_config, _data| {
            let file = PathBuf::from("/tmp/webox-bug5-test.txt");
            fs::write(&file, "hello").unwrap();
            fs::remove_file(&file).unwrap();

            let mut state2 = State { files: HashMap::new() };
            let mut processed2: HashMap<PathBuf, std::time::Instant> = HashMap::new();
            process_file(&file, &mut state2, &mut processed2, Duration::from_millis(500));
            assert!(state2.files.is_empty(), "should not process when canonicalize fails");
            assert!(processed2.is_empty(), "processed should remain empty");
        });
    }

    #[test]
    fn test_processed_cleanup_removes_stale_entries() {
        let mut processed: HashMap<PathBuf, std::time::Instant> = HashMap::new();
        let old_path = PathBuf::from("/tmp/old-file.txt");
        let new_path = PathBuf::from("/tmp/new-file.txt");

        processed.insert(old_path.clone(), std::time::Instant::now() - Duration::from_secs(360));
        processed.insert(new_path.clone(), std::time::Instant::now());

        cleanup_processed(&mut processed, Duration::from_secs(300));

        assert!(!processed.contains_key(&old_path), "old entries should be removed");
        assert!(processed.contains_key(&new_path), "new entries should remain");
    }

    #[test]
    fn test_save_state_not_called_on_failed_copy() {
        with_test_env(|_config, _data| {
            let source = PathBuf::from("/tmp/webox-bug7-copy-fail.txt");
            let _ = fs::remove_file(&source);
            fs::write(&source, "data").unwrap();

            let mut state = State { files: HashMap::new() };
            let mut processed: HashMap<PathBuf, std::time::Instant> = HashMap::new();

            // Make source unreadable so fs::copy fails
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&source).unwrap().permissions();
                perms.set_mode(0o000);
                fs::set_permissions(&source, perms).unwrap();
            }

            process_file(&source, &mut state, &mut processed, Duration::from_millis(500));

            // State and processed should remain unchanged when copy fails
            assert!(state.files.is_empty(), "state should not be modified on failed copy");
            assert!(processed.is_empty(), "processed should not be modified on failed copy");

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(perms) = fs::metadata(&source).map(|m| m.permissions()) {
                    let mut p = perms;
                    p.set_mode(0o644);
                    fs::set_permissions(&source, p).ok();
                }
            }
            fs::remove_file(&source).ok();
        });
    }

    #[test]
    fn test_canonicalize_returns_none_for_nonexistent() {
        let missing = PathBuf::from("/tmp/webox-nonexistent-canon-test.txt");
        assert!(fs::canonicalize(&missing).is_err());
    }

    #[test]
    fn test_event_should_process_filters_metadata_modify() {
        // Metadata modifications should NOT be processed
        assert!(!event_should_process(&EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any))));
        assert!(!event_should_process(&EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime))));
        assert!(!event_should_process(&EventKind::Modify(ModifyKind::Any)));
        assert!(!event_should_process(&EventKind::Modify(ModifyKind::Other)));

        // Data content modifications SHOULD be processed
        assert!(event_should_process(&EventKind::Modify(ModifyKind::Data(DataChange::Content))));
        assert!(event_should_process(&EventKind::Modify(ModifyKind::Data(DataChange::Any))));
        assert!(event_should_process(&EventKind::Modify(ModifyKind::Data(DataChange::Size))));

        // Create events SHOULD be processed
        assert!(event_should_process(&EventKind::Create(CreateKind::Any)));
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_test_env<F>(test_fn: F)
    where
        F: FnOnce(&Path, &Path),
    {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("webox-test-{}", std::process::id()));

        let config = tmp.join("config");
        let data = tmp.join("data");

        fs::create_dir_all(&config).ok();
        fs::create_dir_all(&data).ok();

        env::set_var("XDG_CONFIG_HOME", &config);
        env::set_var("XDG_DATA_HOME", &data);

        test_fn(&config, &data);

        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_copy_to_shared() {
        with_test_env(|_config, _data| {
            let source = PathBuf::from("/tmp/webox-test-source.txt");
            fs::write(&source, "hello").unwrap();

            let mut state = State {
                files: HashMap::new(),
            };

            let dest = copy_to_shared(&source, &mut state).unwrap();
            assert!(dest.exists());
            assert_eq!(fs::read_to_string(&dest).unwrap(), "hello");
            assert_eq!(state.files.len(), 1);
            assert!(dest.starts_with(shared_dir()));

            fs::remove_file(&source).ok();
        });
    }

    #[test]
    fn test_state_persistence() {
        with_test_env(|_config, _data| {
            let mut state = State {
                files: HashMap::new(),
            };
            state.files.insert(
                "test.txt".into(),
                FileEntry {
                    original_name: "test.txt".into(),
                    copied_at: Local::now(),
                    source: PathBuf::from("/tmp/test.txt"),
                },
            );

            save_state(&state);
            assert!(state_path().exists());

            let loaded = load_state();
            assert_eq!(loaded.files.len(), 1);
            assert!(loaded.files.contains_key("test.txt"));
        });
    }

    #[test]
    fn test_empty_state_on_start() {
        with_test_env(|_config, _data| {
            let state = load_state();
            assert!(state.files.is_empty());
        });
    }

    #[test]
    fn test_corrupted_state_warns_and_resets() {
        with_test_env(|config, data| {
            let path = state_path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "{{{corrupted}}}").unwrap();

            // (a) returns empty state
            let state = load_state();
            assert!(state.files.is_empty());

            // (b) warning is printed to stderr
            let exe = std::env::current_exe().unwrap();
            let output = std::process::Command::new(&exe)
                .args(["--nocapture", "--", "corrupted_state_stderr_check"])
                .env("XDG_CONFIG_HOME", config)
                .env("XDG_DATA_HOME", data)
                .output()
                .unwrap();

            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "child test failed:\nstderr: {stderr}",
            );
            assert!(
                stderr.contains("已损坏"),
                "expected warning on stderr, got: {stderr:?}",
            );
        });
    }

    /// Helper test called as subprocess by test_corrupted_state_warns_and_resets.
    /// Expects XDG_CONFIG_HOME / XDG_DATA_HOME to point at a corrupted state dir.
    #[test]
    fn corrupted_state_stderr_check() {
        if std::env::var("XDG_CONFIG_HOME").is_err() {
            eprintln!("SKIP: not invoked via subprocess");
            return;
        }
        // Should produce eprintln! warning when corrupted file is present
        load_state();
    }

    #[test]
    fn test_send_duplicate_source_skips_copy() {
        with_test_env(|_config, _data| {
            let source = PathBuf::from("/tmp/webox-test-dup-src.txt");
            fs::write(&source, "hello").unwrap();

            let mut state = State { files: HashMap::new() };

            let dest1 = copy_to_shared(&source, &mut state).unwrap();
            assert!(dest1.exists());
            assert_eq!(state.files.len(), 1);

            let dest2 = copy_to_shared(&source, &mut state).unwrap();
            assert_eq!(dest2, dest1, "duplicate send should return same path");
            assert_eq!(state.files.len(), 1, "duplicate send should not add another entry");
            assert!(dest2.exists(), "dest should still exist");

            let shared = shared_dir();
            let entries: Vec<_> = fs::read_dir(&shared).unwrap().collect();
            assert_eq!(entries.len(), 1, "only one file should exist in shared dir");

            fs::remove_file(&source).ok();
        });
    }

    #[test]
    fn test_duplicate_filename_handling() {
        with_test_env(|_config, _data| {
            let shared = shared_dir();
            fs::create_dir_all(&shared).unwrap();
            fs::write(shared.join("dup.txt"), "existing").unwrap();

            let source = PathBuf::from("/tmp/dup.txt");
            fs::write(&source, "new content").unwrap();

            let mut state = State {
                files: HashMap::new(),
            };

            let dest = copy_to_shared(&source, &mut state).unwrap();

            let name = dest.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with("dup_"));
            assert_ne!(name, "dup.txt");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "new content");
            assert_eq!(
                fs::read_to_string(shared.join("dup.txt")).unwrap(),
                "existing"
            );

            fs::remove_file(&source).ok();
        });
    }

    #[test]
    fn test_copy_nonexistent_file_fails() {
        with_test_env(|_config, _data| {
            let mut state = State {
                files: HashMap::new(),
            };
            let result =
                copy_to_shared(&PathBuf::from("/tmp/does-not-exist-xyz.txt"), &mut state);
            assert!(result.is_err());
            assert_eq!(state.files.len(), 0);
        });
    }

    #[test]
    fn test_copy_directory_fails() {
        with_test_env(|_config, _data| {
            let dir = PathBuf::from("/tmp/webox-test-dir");
            fs::create_dir_all(&dir).unwrap();
            let mut state = State {
                files: HashMap::new(),
            };
            let result = copy_to_shared(&dir, &mut state);
            assert!(result.is_err());
            fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn test_clean_removes_files() {
        with_test_env(|_config, _data| {
            let shared = shared_dir();
            fs::create_dir_all(&shared).unwrap();
            let file = shared.join("clean-test.txt");
            fs::write(&file, "data").unwrap();

            let mut state = State {
                files: HashMap::new(),
            };
            state.files.insert(
                "clean-test.txt".into(),
                FileEntry {
                    original_name: "clean-test.txt".into(),
                    copied_at: Local::now(),
                    source: PathBuf::from("/tmp/clean-test.txt"),
                },
            );
            save_state(&state);

            cmd_clean(false).unwrap();

            assert!(!file.exists());
            let loaded = load_state();
            assert!(loaded.files.is_empty());
        });
    }

    #[test]
    fn test_clean_nothing_is_ok() {
        with_test_env(|_config, _data| {
            assert!(cmd_clean(false).is_ok());
        });
    }

    #[test]
    fn test_list_with_no_files() {
        with_test_env(|_config, _data| {
            assert!(cmd_list().is_ok());
        });
    }

    #[test]
    fn test_send_reports_summary() {
        with_test_env(|config, data| {
            let valid = PathBuf::from("/tmp/webox-test-summary-valid.txt");
            fs::write(&valid, "content").unwrap();

            let exe = std::env::current_exe().unwrap();
            let output = std::process::Command::new(&exe)
                .args(["--nocapture", "--", "send_summary_subprocess_check"])
                .env("XDG_CONFIG_HOME", config)
                .env("XDG_DATA_HOME", data)
                .output()
                .unwrap();

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "child test failed:\nstdout: {stdout}\nstderr: {stderr}",
            );
            assert!(
                stdout.contains("已完成: 1, 失败: 1"),
                "expected summary in stdout, got: {stdout:?}",
            );
            assert!(
                stderr.contains("✗"),
                "expected file-not-found error on stderr, got: {stderr:?}",
            );

            fs::remove_file(&valid).ok();
        });
    }

    #[test]
    fn send_summary_subprocess_check() {
        if std::env::var("XDG_CONFIG_HOME").is_err() {
            eprintln!("SKIP: not invoked via subprocess");
            return;
        }
        let valid = PathBuf::from("/tmp/webox-test-summary-valid.txt");
        let invalid = PathBuf::from("/tmp/ce4a7b93-e132-4959-8192-b9ad54c135b0-nonexistent.txt");
        cmd_send(&[valid, invalid]).ok();
    }

    #[test]
    fn test_send_multiple_files() {
        with_test_env(|_config, _data| {
            let f1 = PathBuf::from("/tmp/webox-test-a.txt");
            let f2 = PathBuf::from("/tmp/webox-test-b.txt");
            fs::write(&f1, "a").unwrap();
            fs::write(&f2, "b").unwrap();

            cmd_send(&[f1.clone(), f2.clone()]).unwrap();

            let state = load_state();
            assert_eq!(state.files.len(), 2);

            fs::remove_file(&f1).ok();
            fs::remove_file(&f2).ok();
        });
    }
}
