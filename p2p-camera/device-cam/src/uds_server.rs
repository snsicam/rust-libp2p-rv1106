//! 本地 UDS 命令服务
//!
//! 监听 `/tmp/cam_ctrl.sock`, 接收 RV1106 本机另一程序下发的抓拍/合成命令:
//!   {"cmd":"snapshot"}              -> 抓拍一张 JPG 到 /userdata/snaps/
//!   {"cmd":"compose","fps":N}       -> 用 ffmpeg 把 /userdata/snaps/*.jpg 合成 AVI
//!
//! 安全: socket 创建后 chmod 0600, 仅属主可读写; 命令仅两个白名单动作。

use serde::Deserialize;
use std::os::unix::fs::PermissionsExt;
use tokio::net::{UnixListener, UnixStream};
use tracing;

const SOCK_PATH: &str = "/tmp/cam_ctrl.sock";

#[derive(Debug, Deserialize)]
struct UdsCommand {
    cmd: String,
    #[serde(default)]
    fps: Option<u32>,
}

/// 启动 UDS 监听线程 (fire-and-forget)
pub fn spawn() {
    tokio::spawn(async {
        if let Err(e) = run().await {
            tracing::error!("[UdsServer] exited: {e}");
        }
    });
}

async fn run() -> anyhow::Result<()> {
    // 清理遗留 socket
    let _ = std::fs::remove_file(SOCK_PATH);

    let listener = UnixListener::bind(SOCK_PATH)?;
    // 仅属主可读写
    if let Ok(meta) = std::fs::metadata(SOCK_PATH) {
        let mut perm = meta.permissions();
        perm.set_mode(0o600);
        let _ = std::fs::set_permissions(SOCK_PATH, perm);
    }
    tracing::info!("[UdsServer] listening on {SOCK_PATH}");

    loop {
        let (stream, _addr) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream).await {
                tracing::warn!("[UdsServer] conn error: {e}");
            }
        });
    }
}

async fn handle_conn(mut stream: UnixStream) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&buf[..n]);
    let resp = match serde_json::from_str::<UdsCommand>(&text) {
        Ok(cmd) => dispatch(cmd).await,
        Err(e) => format!("{{\"ok\":false,\"error\":\"{e}\"}}\n"),
    };

    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn dispatch(cmd: UdsCommand) -> String {
    match cmd.cmd.as_str() {
        "snapshot" => {
            #[cfg(feature = "rv1106")]
            {
                match crate::rk_video_source::take_snapshot() {
                    Ok(path) => format!("{{\"ok\":true,\"file\":\"{path}\"}}\n"),
                    Err(e) => format!("{{\"ok\":false,\"error\":\"{e}\"}}\n"),
                }
            }
            #[cfg(not(feature = "rv1106"))]
            {
                format!("{{\"ok\":false,\"error\":\"not available on this platform\"}}\n")
            }
        }
        "compose" => {
            let fps = cmd.fps.unwrap_or(5);
            #[cfg(feature = "rv1106")]
            {
                match crate::rk_video_source::compose_video(fps) {
                    Ok(file) => format!("{{\"ok\":true,\"file\":\"{file}\"}}\n"),
                    Err(e) => format!("{{\"ok\":false,\"error\":\"{e}\"}}\n"),
                }
            }
            #[cfg(not(feature = "rv1106"))]
            {
                format!("{{\"ok\":false,\"error\":\"not available on this platform\"}}\n")
            }
        }
        other => format!("{{\"ok\":false,\"error\":\"unknown cmd: {other}\"}}\n"),
    }
}
