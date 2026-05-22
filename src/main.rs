use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use notify::{Event, EventKind, RecursiveMode, Watcher};
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
#[derive(Debug, Serialize, Deserialize)]
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
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| State {
                files: HashMap::new(),
            })
    } else {
        State {
            files: HashMap::new(),
        }
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
    for path in paths {
        match copy_to_shared(path, &mut state) {
            Ok(_) => {}
            Err(e) => eprintln!("✗ {}: {}", path.display(), e),
        }
    }
    save_state(&state);
    Ok(())
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

    // 过滤不应处理的文件
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

    // 尝试处理一个文件路径
    let try_process = |path: &Path, state: &mut State, processed: &mut HashMap<PathBuf, std::time::Instant>| {
        if !path.is_file() || should_skip(path) {
            return;
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let now = std::time::Instant::now();

        // 如果最近处理过这个文件，跳过
        if let Some(last) = processed.get(&canonical) {
            if now.duration_since(*last) < debounce {
                return;
            }
        }

        if let Err(e) = copy_to_shared(path, state) {
            eprintln!("✗ {}: {}", path.display(), e);
        }
        processed.insert(canonical, now);
        save_state(state);
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
                EventKind::Create(_) | EventKind::Modify(_) => {
                    for path in &event.paths {
                        // 等文件写完再处理
                        std::thread::sleep(Duration::from_millis(300));
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
