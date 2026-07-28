//! JNI 桥接 — Android (Kotlin/Java) 调用 Rust MediaPlayer 的接口
//!
//! Kotlin 侧对应类: com.p2pcamera.mediaplayer.RustBridge
//!
//! API:
//!   nativeCreate()                  -> Long handle
//!   nativeConnect(handle, json)     -> Boolean
//!   nativePollVideoFrame(handle)    -> byte[] or null  (PTS 8B + flags 1B + H.265 data)
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
    DirectUpgraded {
        connection_type: String,
    },
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
        serial_map: std::collections::HashMap<String, String>,
        stream_type: String,
        no_audio: bool,
        network_type: String,
    },
    Control {
        request_json: String,
        response_tx: Sender<String>,
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
    /// 关闭信号：nativeDestroy 时发送，令后台线程退出。
    /// 否则线程持有 video_tx/event_tx 的 clone，通道永不饱和，线程永不退出，
    /// 旧设备连接泄露（切设备后出现"两个设备同时连接"）。
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
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
    // 关闭信号：nativeDestroy 时发送，令后台线程退出（否则线程持有发送端 clone 永不退出）
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

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
                Ok(Cmd::Connect { relays, device_id: did, enable_mdns, serial_map, stream_type, no_audio, network_type: nt }) => {
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
                        .connect(&relay_strs, &did, enable_mdns, &serial_map, &stream_type)
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
                Ok(Cmd::Control { response_tx, .. }) => {
                    // 控制命令在连接前到达，返回错误并继续等待 Connect
                    let err_resp = proto::control::ControlResponse::err("not connected");
                    let _ = response_tx.send(serde_json::to_string(&err_resp).unwrap_or_default());
                    // 继续等待 Connect 命令
                    loop {
                        match cmd_rx.recv() {
                            Ok(Cmd::Connect { relays, device_id: did, enable_mdns, serial_map, stream_type, no_audio, network_type: nt }) => {
                                device_id = did.clone();
                                network_type = nt;
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
                                    .connect(&relay_strs, &did, enable_mdns, &serial_map, &stream_type)
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
                                if no_audio {
                                    drop(audio_tx.take());
                                }
                                break;
                            }
                            Ok(Cmd::Control { response_tx, .. }) => {
                                let err_resp = proto::control::ControlResponse::err("not connected");
                                let _ = response_tx.send(serde_json::to_string(&err_resp).unwrap_or_default());
                            }
                            Err(_) => return,
                        }
                    }
                }
                Err(_) => return, // channel closed
            }

            // 主事件循环: 驱动 swarm + 轮询帧 + 检测断连 + 自动重连 + 处理控制命令
            loop {
                tokio::select! {
                    _ = viewer.poll_swarm() => {
                        // swarm 事件已在 viewer 内部处理
                    }
                    _ = &mut shutdown_rx => {
                        tracing::info!("[JNI] Shutdown requested, exiting viewer thread");
                        return;
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

                        // 处理控制命令 (非阻塞轮询)
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            match cmd {
                                Cmd::Control { request_json, response_tx } => {
                                    let req: proto::control::ControlRequest = match serde_json::from_str(&request_json) {
                                        Ok(r) => r,
                                        Err(e) => {
                                            let err_resp = proto::control::ControlResponse::err(&format!("invalid json: {e}"));
                                            let _ = response_tx.send(serde_json::to_string(&err_resp).unwrap_or_default());
                                            continue;
                                        }
                                    };

                                    match viewer.send_control(&req).await {
                                        Ok(resp) => {
                                            let _ = response_tx.send(serde_json::to_string(&resp).unwrap_or_default());
                                        }
                                        Err(e) => {
                                            let err_resp = proto::control::ControlResponse::err(&e.to_string());
                                            let _ = response_tx.send(serde_json::to_string(&err_resp).unwrap_or_default());
                                        }
                                    }
                                }
                                Cmd::Connect { .. } => {
                                    // Connect 命令已在循环前处理，忽略
                                }
                            }
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
                                    // 通知 Android 侧直连升级 (从 sub 流切换到 main 流)
                                    let _ = event_tx.send(ViewerEvent::DirectUpgraded {
                                        connection_type: conn_type.to_string(),
                                    });
                                }
                                MediaPlayerEvent::StreamEOF { reason } => {
                                    tracing::warn!("[JNI] Stream EOF: {reason}");
                                    // StreamEOF 视同断连，触发重连
                                    let _ = event_tx.send(ViewerEvent::Disconnected);
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
                                MediaPlayerEvent::NatDiagnosis { local_nat, remote_nat } => {
                                    tracing::info!("[JNI] NAT diagnosis: local={}, remote={}",
                                        local_nat.short_name(),
                                        remote_nat.as_deref().unwrap_or("Unknown"));
                                }
                                MediaPlayerEvent::DcutrBackoff => {
                                    tracing::warn!("[JNI] DCUtR backoff, disabling DCUtR for next reconnect");
                                    viewer.set_enable_dcutr(false);
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
        shutdown_tx,
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

    // 可选: 本地 serial→peer_id 静态映射 (JSON object)。命中时无需 Relay 即可解析 SN。
    let serial_map: std::collections::HashMap<String, String> = config["serial_map"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

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
            .map(|h| h.cmd_tx.send(Cmd::Connect { relays, device_id, enable_mdns, serial_map, stream_type, no_audio, network_type }).is_ok())
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
/// 返回格式: [PTS(8 bytes, big-endian i64, µs)] + [flags(1 byte)] + [H.265 NAL data]
///   - flags bit 2 (0x04, FLAG_VIDEO_KEYFRAME) = 关键帧, 由 viewer 接收端字节扫描判定
///     (cam 不计算/不传该标志, 实测不可靠), 此处为唯一权威值。
///     原生 APP 用 `(flags & 0x04) != 0` 判定关键帧。
/// 无帧时返回 null
///
/// Kotlin 侧解包:
/// ```kotlin
/// val buf = java.nio.ByteBuffer.wrap(data).order(java.nio.ByteOrder.BIG_ENDIAN)
/// val ptsUs = buf.long                       // 8 bytes
/// val flags = buf.get().toInt() and 0xFF     // 1 byte
/// val isKeyframe = (flags and 0x04) != 0     // FLAG_VIDEO_KEYFRAME
/// val frameData = data.sliceArray(9 until data.size)
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
            // flags 字节: 含关键帧 bit (FLAG_VIDEO_KEYFRAME=0x04), 由 viewer 接收端字节扫描判定
            // (cam 不计算/不传, 实测不可靠), 此处为唯一权威值。原生 APP 用 (flags & 0x04) != 0 判定关键帧。
            let flags = packet.flags;
            let total_len = (8 + 1 + data.len()) as i32;

            match env.new_byte_array(total_len) {
                Ok(arr) => {
                    // 写 PTS 前缀 (8 bytes, big-endian)
                    let pts_i8: &[i8] = unsafe {
                        std::slice::from_raw_parts(pts_bytes.as_ptr() as *const i8, 8)
                    };
                    let _ = env.set_byte_array_region(&arr, 0, pts_i8);
                    // 写 flags (1 byte): bit 2 (0x04) = 关键帧
                    let flags_i8: &[i8] = unsafe {
                        std::slice::from_raw_parts(&flags as *const u8 as *const i8, 1)
                    };
                    let _ = env.set_byte_array_region(&arr, 8, flags_i8);
                    // 写帧数据 (从 offset 9 开始)
                    let data_i8: &[i8] = unsafe {
                        std::slice::from_raw_parts(data.as_ptr() as *const i8, data.len())
                    };
                    let _ = env.set_byte_array_region(&arr, 9, data_i8);
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
    if let Some(h) = take_handle(idx) {
        // 发送关闭信号，令后台线程退出并断开旧设备连接
        let _ = h.shutdown_tx.send(());
        // h 在此作用域结束时 drop，关闭 cmd_tx 等
    }
}

/// 发送控制命令
///
/// Kotlin: external fun nativeSendControlCommand(handle: Long, json: String): String
///
/// json: ControlRequest JSON (如 {"type":"get_encoder_config","stream":"main"})
/// 返回: ControlResponse JSON (如 {"ok":true,"encoder_config":{...}})
#[no_mangle]
pub extern "system" fn Java_com_p2pcamera_mediaplayer_RustBridge_nativeSendControlCommand(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    json: JString,
) -> jstring {
    let idx = match handle_to_idx(handle) {
        Some(i) => i,
        None => {
            let err = proto::control::ControlResponse::err("invalid handle");
            return json_response(&mut env, &err);
        }
    };

    let json_str: String = match env.get_string(&json) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/IllegalArgumentException", format!("{e}"));
            let err = proto::control::ControlResponse::err("invalid json string");
            return json_response(&mut env, &err);
        }
    };

    // 创建响应通道
    let (response_tx, response_rx) = bounded::<String>(1);

    // 发送命令到后台线程
    let sent = with_handles(|handles| {
        handles
            .get(idx)
            .and_then(|h| h.as_ref())
            .map(|h| h.cmd_tx.send(Cmd::Control {
                request_json: json_str,
                response_tx: response_tx.clone(),
            }).is_ok())
            .unwrap_or(false)
    });

    if !sent {
        let err = proto::control::ControlResponse::err("failed to send control command");
        return json_response(&mut env, &err);
    }

    // 等待响应 (5s 超时)
    match response_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(resp_json) => {
            match env.new_string(&resp_json) {
                Ok(s) => s.into_raw(),
                Err(_) => std::ptr::null_mut(),
            }
        }
        Err(_) => {
            let err = proto::control::ControlResponse::err("control command timeout");
            json_response(&mut env, &err)
        }
    }
}

/// 将 ControlResponse 序列化为 JSON 并创建 jstring
fn json_response(env: &mut JNIEnv, resp: &proto::control::ControlResponse) -> jstring {
    let json = serde_json::to_string(resp).unwrap_or_else(|_| r#"{"ok":false,"error":"serialize error"}"#.to_string());
    match env.new_string(&json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}