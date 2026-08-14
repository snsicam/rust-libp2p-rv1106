//! RV1106 本地抓拍/合成测试程序
//!
//! 经 UDS `/tmp/cam_ctrl.sock` 向 device-cam 下发命令:
//!   cam-test snapshot           抓拍一张 JPG 到 /userdata/snaps/
//!   cam-test compose [fps]      用 ffmpeg 把 /userdata/snaps/*.jpg 合成 MOV (MJPEG,默认 fps=5)
//!   cam-test loop [n] [fps] [interval]  循环抓拍 n 次(间隔 interval 秒,默认1)后合成
//!
//! 调试快捷: 也可直接 `echo '{"cmd":"snapshot"}' | nc -U /tmp/cam_ctrl.sock`

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const SOCK: &str = "/tmp/cam_ctrl.sock";

#[derive(Serialize)]
struct Cmd {
    cmd: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    fps: Option<u32>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return Ok(());
    }

    let (cmd, fps) = match args[0].as_str() {
        "snapshot" => (Cmd { cmd: "snapshot", fps: None }, None),
        "compose" => {
            let fps = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
            (Cmd { cmd: "compose", fps: Some(fps) }, Some(fps))
        }
        "loop" => {
            let n: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
            let fps = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let interval: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1.0);
            return run_loop(n, fps, interval).await;
        }
        other => {
            eprintln!("未知命令: {other}");
            print_usage();
            return Ok(());
        }
    };

    let json = serde_json::to_string(&cmd)?;
    println!("[cam-test] -> {json}");
    let resp = send(&json).await?;
    println!("[cam-test] <- {resp}");
    let _ = fps;
    Ok(())
}

async fn run_loop(n: u32, fps: u32, interval: f64) -> anyhow::Result<()> {
    let interval = if interval > 0.0 {
        interval
    } else {
        eprintln!("[cam-test] 间隔必须大于 0,已回退为 1.0s");
        1.0
    };
    for i in 1..=n {
        let json = serde_json::to_string(&Cmd { cmd: "snapshot", fps: None })?;
        println!("[cam-test] ({i}/{n}) snapshot");
        let resp = send(&json).await?;
        println!("[cam-test] <- {resp}");
        if i < n {
            tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;
        }
    }
    let json = serde_json::to_string(&Cmd { cmd: "compose", fps: Some(fps) })?;
    println!("[cam-test] compose fps={fps}");
    let resp = send(&json).await?;
    println!("[cam-test] <- {resp}");
    Ok(())
}

async fn send(json: &str) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(SOCK).await?;
    stream.write_all(json.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

fn print_usage() {
    println!("用法:");
    println!("  cam-test snapshot           抓拍一张 JPG");
    println!("  cam-test compose [fps]      合成 MOV (默认 fps=5)");
    println!("  cam-test loop [n] [fps] [interval]  循环抓拍 n 次后合成 (默认 n=5, fps=5, 间隔1s)");
}
