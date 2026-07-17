//! JNI 桥接 — Android (Kotlin/Java) 调用 Rust MediaPlayer 的接口
//!
//! Kotlin 侧对应类: com.p2pcamera.mediaplayer.RustBridge
//!
//! API:
//!   nativeCreate()                  -> Long handle
//!   nativeConnect(handle, json)     -> Boolean
//!   nativePollVideoFrame(handle)    -> byte[] or null  (PTS prefix + H.265 data)
//!   nativePollAudioFrame(handle)    -> byte[] or null  (PTS prefix + PCM data)
//!   nativePollEvent(handle)         -> String or null
//!   nativeDestroy(handle)           -> void

use std::sync::Mutex;

use crossbeam_channel::{bounded, Receiver, Sender};
use jni::{
    objects::{JClass, JString},
    sys::{jboolean, jbyteArray, jlong, jstring, JNI_FALSE, JNI_TRUE},
    JNIEnv,
};
use serde::Serialize;
use tokio::runtime::Runtime;

use crate::viewer::{MediaPlayer, MediaPlayerEvent};
use proto::media_packet::MediaPacket;

// ── Event 类型 ──

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
enum ViewerEvent {
    Connecting,
    Connected {
        peer_id: String,
        connection_type: String,
    },
    StreamReady,
    Disconnected,
    Error {
        message: String,
    },
}

// ── 后台线程命令 ──

enum Cmd {
    Connect {
        relays: Vec<String>,
        device_id: String,
        enable_mdns: bool,
        stream_type: String,
        no_audio: bool,
        network_type: String,
    },
}

// ── ViewerHandle ──

struct ViewerHandle {
    /// 视频帧通道（Rust → Android 轮询）
    video_rx: Receiver<MediaPacket>,
    /// 音频帧通道（Rust → Android 轮询）
    audio_rx: Receiver<MediaPacket>,
    /// 事件通道
    event_rx: Receiver<ViewerEvent>,
    /// 命令通道
    cmd_tx: Sender<Cmd>,
}

// ── 全局句柄表 ──

static HANDLES: Mutex<Option<Vec<Option<ViewerHandle>>>> = Mutex::new(None);

fn with_handles<F, R>(f: F) -> R
where
    F: FnOnce(&mut Vec<Option<ViewerHandle>>) -> R,
{
    let mut guard = HANDLES.lock().unwrap();
    if guard.is_none() {
        *guard = Some(Vec::new());
    }
    f(guard.as_mut().unwrap())
}

fn alloc_handle(handle: ViewerHandle) -> usize {
    with_handles(|handles| {
        if let Some(pos) = handles.iter().position(|h| h.is_none()) {
            handles[pos] = Some(handle);
            pos + 1 // 返回 1-based 索引，0 表示无效句柄
        } else {
            handles.push(Some(handle));
            handles.len() // len 已经是 1-based
        }
    })
}

fn take_handle(idx: usize) -> Option<ViewerHandle> {
    if idx == 0 { return None; }
    with_handles(|handles| handles.get_mut(idx - 1)?.take())
}

/// 将 JNI handle (1-based) 转为 Vec 索引 (0-based)
fn handle_to_idx(handle: jlong) -> Option<usize> {
    let idx = handle as usize;
    if idx == 0 { return None; }
    Some(idx - 1)
}

// ═══════════════════════════════════════════════════════════════════
// JNI 导出函数
// ═══════════════════════════════════════════════════════════════════

/// 创建 Viewer 实例
///
/// Kotlin: external fun nativeCreate(): Long
#[no_mangle]
pub extern "system" fn Java_com_p2pcamera_mediaplayer_RustBridge_nativeCreate(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    let (video_tx, video_rx) = bounded::<MediaPacket>(120);
    let (audio_tx, audio_rx) = bounded::<MediaPacket>(200);
    let (event_tx, event_rx) = bounded::<ViewerEvent>(32);
    let (cmd_tx, cmd_rx) = bounded::<Cmd>(4);

    // 启动后台 tokio runtime，驱动 MediaPlayer
    std::thread::spawn(move || {
        let rt = match Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = event_tx.send(ViewerEvent::Error {
                    message: format!("tokio runtime: {e}"),
                });
                return;
            }
        };

        rt.block_on(async {
            // audio_tx 用 Option 包装，no_audio 时 take() 关闭发送端
            let mut audio_tx = Some(audio_tx);

            // 创建 viewer（是否启用 DCUtR 取决于网络类型：4G/CGNAT 下禁用）
            let mut viewer;

            // 等待连接命令
            let device_id;
            let network_type;
            match cmd_rx.recv() {
                Ok(Cmd::Connect { relays, device_id: did, enable_mdns, stream_type, no_audio, network_type: nt }) => {
                    device_id = did.clone();
                    network_type = nt;
                    // 默认启用 DCUtR 打洞：锥形/EIM NAT（含多数 4G）可打洞成功，省中继带宽。
                    // 仅当连接后 net_diag 确认为 Symmetric NAT 时，才在重连时禁用
                    // （viewer.rs 会重建 Swarm 并去掉 dcutr 行为），避免每次重连都无效打洞 ~17s。
                    let enable_dcutr = true;
                    println!("[Viewer] DCUtR enabled (network_type={}, will auto-disable on reconnect if Symmetric NAT detected)", network_type);
                    viewer = match MediaPlayer::new(enable_dcutr).await {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = event_tx.send(ViewerEvent::Error {
                                message: format!("create viewer: {e}"),
                            });
                            return;
                        }
                    };
                    let _ = event_tx.send(ViewerEvent::Connecting);
                    let relay_strs: Vec<String> = relays.clone();
                    match viewer
                        .connect(&relay_strs, &did, enable_mdns, &stream_type)
                        .await
                    {
                        Ok(()) => {
                            let _ = event_tx.send(ViewerEvent::Connected {
                                peer_id: did,
                                connection_type: "relay".into(),
                            });
                            let _ = event_tx.send(ViewerEvent::StreamReady);
                        }
                        Err(e) => {
                            let _ = event_tx.send(ViewerEvent::Error {
                                message: format!("connect failed: {e}"),
                            });
                            return;
                        }
                    }

                    // 如果 no_audio，关闭音频发送端（Android 侧 pollAudioFrame 将返回 null）
                    if no_audio {
                        drop(audio_tx.take());
                    }
                }
                Err(_) => return, // channel closed
            }

            // 主事件循环: 驱动 swarm + 轮询帧 + 检测断连 + 自动重连
            loop {
                tokio::select! {
                    _ = viewer.poll_swarm() => {
                        // swarm 事件已在 viewer 内部处理
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(5)) => {
                        // 定期拉视频帧 → 发送到 Android 侧
                        while let Some(frame) = viewer.poll_video_frame() {
                            if video_tx.send(frame).is_err() {
                                return; // Android 侧已销毁
                            }
                        }
                        // 定期拉音频帧 → 发送到 Android 侧
                        // (如果 no_audio 已 take audio_tx，poll 结果会被忽略)
                        if let Some(tx) = audio_tx.as_ref() {
                            while let Some(frame) = viewer.poll_audio_frame() {
                                if tx.send(frame).is_err() {
                                    // Android 侧已销毁
                                    return;
                                }
                            }
                        } else {
                            // no_audio 模式：丢弃音频帧
                            while viewer.poll_audio_frame().is_some() {}
                        }

                        // 检测 MediaPlayer 内部事件（断连/直连升级）
                        while let Some(event) = viewer.poll_event() {
                            match event {
                                MediaPlayerEvent::Disconnected { reason } => {
                                    tracing::warn!("[JNI] Disconnected: {reason}");
                                    let _ = event_tx.send(ViewerEvent::Disconnected);

                                    // 自动重连
                                    tracing::info!("[JNI] Auto-reconnecting...");
                                    let _ = event_tx.send(ViewerEvent::Connecting);
                                    match viewer.reconnect().await {
                                        Ok(()) => {
                                            let _ = event_tx.send(ViewerEvent::Connected {
                                                peer_id: device_id.clone(),
                                                connection_type: "relay".into(),
                                            });
                                            let _ = event_tx.send(ViewerEvent::StreamReady);
                                            tracing::info!("[JNI] Reconnected successfully");
                                        }
                                        Err(e) => {
                                            tracing::error!("[JNI] Reconnect failed: {e}");
                                            let _ = event_tx.send(ViewerEvent::Error {
                                                message: format!("reconnect failed: {e}"),
                                            });
                                            return;
                                        }
                                    }
                                }
                                MediaPlayerEvent::DirectUpgraded { via_lan } => {
                                    let conn_type = if via_lan { "LAN direct" } else { "DCUtR" };
                                    tracing::info!("[JNI] Direct upgraded: {conn_type}");
                                }
                            }
                        }
                    }
                }
            }
        });
    });

    alloc_handle(ViewerHandle {
        video_rx,
        audio_rx,
        event_rx,
        cmd_tx,
    }) as jlong
}

/// 连接到设备
///
/// Kotlin: external fun nativeConnect(handle: Long, json: String): Boolean
///
/// json: {"relays":["/ip4/.../tcp/.../p2p/...","/ip4/..."],"deviceId":"12D3..."}
#[no_mangle]
pub extern "system" fn Java_com_p2pcamera_mediaplayer_RustBridge_nativeConnect(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    json: JString,
) -> jboolean {
    let idx = match handle_to_idx(handle) {
        Some(i) => i,
        None => return JNI_FALSE,
    };

    let json_str: String = match env.get_string(&json) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/IllegalArgumentException", format!("{e}"));
            return JNI_FALSE;
        }
    };

    let config: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(e) => {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                format!("invalid json: {e}"),
            );
            return JNI_FALSE;
        }
    };

    let relays: Vec<String> = config["relays"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let device_id = config["deviceId"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let enable_mdns = config["enable_mdns"].as_bool().unwrap_or(false);

    let stream_type = config["stream_type"]
        .as_str()
        .unwrap_or("auto")
        .to_string();

    let no_audio = config["no_audio"].as_bool().unwrap_or(false);

    let network_type = config["network_type"]
        .as_str()
        .unwrap_or("auto")
        .to_string();

    if relays.is_empty() || device_id.is_empty() {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            "relays and deviceId are required",
        );
        return JNI_FALSE;
    }

    let sent = with_handles(|handles| {
        handles
            .get(idx)
            .and_then(|h| h.as_ref())
            .map(|h| h.cmd_tx.send(Cmd::Connect { relays, device_id, enable_mdns, stream_type, no_audio, network_type }).is_ok())
            .unwrap_or(false)
    });

    if sent {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

/// 轮询视频帧（非阻塞）
///
/// Kotlin: external fun nativePollVideoFrame(handle: Long): ByteArray?
///
/// 返回格式: [PTS(8 bytes, big-endian i64, µs)] + [H.265 NAL data]
/// 无帧时返回 null
///
/// Kotlin 侧解包:
/// ```kotlin
/// val buf = java.nio.ByteBuffer.wrap(data).order(java.nio.ByteOrder.BIG_ENDIAN)
/// val ptsUs = buf.long
/// val frameData = data.sliceArray(8 until data.size)
/// ```
#[no_mangle]
pub extern "system" fn Java_com_p2pcamera_mediaplayer_RustBridge_nativePollVideoFrame(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jbyteArray {
    let idx = match handle_to_idx(handle) {
        Some(i) => i,
        None => return std::ptr::null_mut(),
    };
    let frame = with_handles(|handles| {
        handles
            .get(idx)
            .and_then(|h| h.as_ref())
            .and_then(|h| h.video_rx.try_recv().ok())
    });

    match frame {
        Some(packet) => {
            let data = &packet.data;
            let pts_us = (packet.timestamp_ms * 1000) as i64;
            let pts_bytes = pts_us.to_be_bytes();
            let total_len = (8 + data.len()) as i32;

            match env.new_byte_array(total_len) {
                Ok(arr) => {
                    // 写 PTS 前缀 (8 bytes, big-endian)
                    let pts_i8: &[i8] = unsafe {
                        std::slice::from_raw_parts(pts_bytes.as_ptr() as *const i8, 8)
                    };
                    let _ = env.set_byte_array_region(&arr, 0, pts_i8);
                    // 写帧数据
                    let data_i8: &[i8] = unsafe {
                        std::slice::from_raw_parts(data.as_ptr() as *const i8, data.len())
                    };
                    let _ = env.set_byte_array_region(&arr, 8, data_i8);
                    arr.into_raw()
                }
                Err(_) => std::ptr::null_mut(),
            }
        }
        None => std::ptr::null_mut(),
    }
}

/// 轮询音频帧（非阻塞）
///
/// Kotlin: external fun nativePollAudioFrame(handle: Long): ByteArray?
///
/// 返回格式: [PTS(8 bytes, big-endian i64, µs)] + [PCM 16-bit LE data]
/// 无帧时返回 null
///
/// 音频格式: 16kHz, 16-bit PCM mono (当前实现)
/// 可通过 packet.flags 判断编码格式: 0=PCM16LE, 1=AAC, 2=G711A, 3=G711U
///
/// Kotlin 侧解包:
/// ```kotlin
/// val buf = java.nio.ByteBuffer.wrap(data).order(java.nio.ByteOrder.BIG_ENDIAN)
/// val ptsUs = buf.long
/// val pcmData = data.sliceArray(8 until data.size)
/// ```
#[no_mangle]
pub extern "system" fn Java_com_p2pcamera_mediaplayer_RustBridge_nativePollAudioFrame(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jbyteArray {
    let idx = match handle_to_idx(handle) {
        Some(i) => i,
        None => return std::ptr::null_mut(),
    };
    let frame = with_handles(|handles| {
        handles
            .get(idx)
            .and_then(|h| h.as_ref())
            .and_then(|h| h.audio_rx.try_recv().ok())
    });

    match frame {
        Some(packet) => {
            let data = &packet.data;
            let pts_us = (packet.timestamp_ms * 1000) as i64;
            let pts_bytes = pts_us.to_be_bytes();
            let total_len = (8 + data.len()) as i32;

            match env.new_byte_array(total_len) {
                Ok(arr) => {
                    // 写 PTS 前缀 (8 bytes, big-endian)
                    let pts_i8: &[i8] = unsafe {
                        std::slice::from_raw_parts(pts_bytes.as_ptr() as *const i8, 8)
                    };
                    let _ = env.set_byte_array_region(&arr, 0, pts_i8);
                    // 写音频数据
                    let data_i8: &[i8] = unsafe {
                        std::slice::from_raw_parts(data.as_ptr() as *const i8, data.len())
                    };
                    let _ = env.set_byte_array_region(&arr, 8, data_i8);
                    arr.into_raw()
                }
                Err(_) => std::ptr::null_mut(),
            }
        }
        None => std::ptr::null_mut(),
    }
}

/// 轮询事件
///
/// Kotlin: external fun nativePollEvent(handle: Long): String?
/// 返回 JSON 事件字符串，无事件时返回 null
#[no_mangle]
pub extern "system" fn Java_com_p2pcamera_mediaplayer_RustBridge_nativePollEvent(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let idx = match handle_to_idx(handle) {
        Some(i) => i,
        None => return std::ptr::null_mut(),
    };
    let event = with_handles(|handles| {
        handles
            .get(idx)
            .and_then(|h| h.as_ref())
            .and_then(|h| h.event_rx.try_recv().ok())
    });

    match event {
        Some(ev) => {
            let json = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
            match env.new_string(&json) {
                Ok(s) => s.into_raw(),
                Err(_) => std::ptr::null_mut(),
            }
        }
        None => std::ptr::null_mut(),
    }
}

/// 销毁 Viewer 实例
///
/// Kotlin: external fun nativeDestroy(handle: Long)
#[no_mangle]
pub extern "system" fn Java_com_p2pcamera_mediaplayer_RustBridge_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    let idx = handle as usize;
    drop(take_handle(idx));
}