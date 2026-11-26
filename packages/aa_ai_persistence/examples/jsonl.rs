//! 示例：用 JSONL 作为 `SessionEvent` 的磁盘格式。
//!
//! 这不是库的 API：`aa_ai_persistence` 只定义事件形状和 `SessionLog::from_events`，
//! 磁盘格式由应用自己决定。这里演示一种最简单的做法——一行一个 JSON 事件，
//! 追加写、按行读；应用可以换成 DB、SQLite、对象存储或任何别的东西。
//!
//! 运行：`cargo run -p aa_ai_persistence --example jsonl`

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use aa_ai_persistence::{Header, SessionEvent, SessionLog, SessionStore};

/// JSONL 存储实现库定义的 `SessionStore` 契约。
struct JsonlStore {
    path: PathBuf,
}

impl SessionStore for JsonlStore {
    type Error = io::Error;

    fn load(&self) -> Result<Vec<SessionEvent>, Self::Error> {
        load_jsonl(&self.path)
    }

    fn append(&self, events: &[SessionEvent]) -> Result<(), Self::Error> {
        append_jsonl(&self.path, events)
    }
}

/// 把新增事件追加到文件（一行一个 JSON，不含数组包裹）。
fn append_jsonl(path: &Path, events: &[SessionEvent]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut buffer = String::new();
    for event in events {
        // serde_json 是示例自己选的编码器，库并不强制。
        buffer.push_str(&serde_json::to_string(event).map_err(io::Error::other)?);
        buffer.push('\n');
    }
    file.write_all(buffer.as_bytes())?;
    file.flush()
}

/// 读回事件（空行忽略）；半行/坏行在这里明确报错，库不负责猜测修复。
fn load_jsonl(path: &Path) -> io::Result<Vec<SessionEvent>> {
    let text = fs::read_to_string(path)?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: {error}", index + 1),
                )
            })
        })
        .collect()
}

fn main() -> io::Result<()> {
    let mut log = SessionLog::new(Header::new("", "sys")).unwrap();
    log.push_user_text("hello");
    log.push_assistant(aa_ai_persistence::AssistantMessage {
        text: "hi".to_owned(),
        ..Default::default()
    });

    let path = std::env::temp_dir().join("aa-ai-persistence-jsonl-example.jsonl");
    let _ = fs::remove_file(&path);
    let store = JsonlStore { path: path.clone() };
    store.append(log.events())?;

    let restored = SessionLog::from_events(store.load()?)
        .map_err(|error| io::Error::other(error.to_string()))?;
    // 事实源一致；生效历史由事件按需投影，不需要另存一份。
    assert_eq!(restored.events(), log.events());
    let messages = restored.messages();
    assert_eq!(messages.len(), 3); // system → user → assistant
    println!(
        "events={} messages={}",
        restored.events().len(),
        messages.len()
    );

    fs::remove_file(&path)?;
    Ok(())
}
