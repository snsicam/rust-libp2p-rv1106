//! Relay 上的相机"短序列号 → 真实 PeerId"签名注册表协议
//!
//! 设计背景见 p2p-camera 设计讨论 (B 方案):
//! 相机板载序列号 (如树莓派 `/proc/cpuinfo` 的 `Serial`) 是**公开、低熵**的 8 字节值，
//! 不能用作密钥派生源 (否则任何人可读/可暴破，冒充相机)。但它很适合做**公开的查找键**。
//!
//! 因此流程为:
//! 1. 相机用自己**持久化、独享**的 ed25519 私钥，对 `(serial || peer_id)` 签名；
//! 2. 通过 [`REGISTRY_PROTOCOL`] 流把 `(serial, peer_id, 公钥, 签名)` 注册到 relay；
//! 3. viewer 只知 serial，向 relay 查询 → 拿回 `(peer_id, 公钥, 签名)`；
//! 4. viewer 用返回的公钥验签，确认绑定真实无误，再用真实 peer_id 拨通相机。
//!
//! 安全性: `(serial→peer_id)` 绑定由相机私钥签名，公开 serial 也无法被第三方抢注伪造；
//! 相机私钥始终只在相机本机，viewer 拿到的 peer_id 真实、不可伪造。

use anyhow::{anyhow, Result};
use bytes::{Buf, BufMut, BytesMut};
use libp2p_swarm::StreamProtocol;

/// 应用层协议名: 相机/viewer 与 relay 之间的注册表流
pub const REGISTRY_PROTOCOL: StreamProtocol =
    StreamProtocol::new("/p2p-camera/registry/1.0.0");

// 消息类型标签
const MSG_REGISTER: u8 = 1;
const MSG_QUERY: u8 = 2;
const MSG_RESPONSE: u8 = 3;
const MSG_NOT_FOUND: u8 = 4;
const MSG_ERROR: u8 = 5;

/// 注册表消息 (所有字段为裸字节，签名/公钥格式见各调用方)
#[derive(Debug, Clone)]
pub enum RegistryMessage {
    /// 相机 → relay: 注册绑定
    /// `peer_id` / `pubkey` 均为 protobuf 编码的二进制
    /// 签名 = ed25519(privkey, serial_bytes || peer_id_bytes)
    Register {
        serial: String,
        peer_id: Vec<u8>,
        pubkey: Vec<u8>,
        signature: Vec<u8>,
    },
    /// viewer → relay: 按 serial 查询
    Query { serial: String },
    /// relay → viewer: 查询命中
    Response {
        peer_id: Vec<u8>,
        pubkey: Vec<u8>,
        signature: Vec<u8>,
    },
    /// relay → viewer: 未找到该 serial
    NotFound,
    /// relay → 对端: 错误说明
    Error { message: String },
}

fn write_len_prefixed(buf: &mut BytesMut, data: &[u8]) {
    buf.put_slice(&(data.len() as u16).to_be_bytes());
    buf.put_slice(data);
}

fn read_len_prefixed(buf: &mut &[u8]) -> Result<Vec<u8>> {
    if buf.len() < 2 {
        return Err(anyhow!("registry: truncated length prefix"));
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    buf.advance(2);
    if buf.len() < len {
        return Err(anyhow!(
            "registry: truncated payload (need {len}, have {})",
            buf.len()
        ));
    }
    let data = buf[..len].to_vec();
    buf.advance(len);
    Ok(data)
}

impl RegistryMessage {
    /// 序列化为长度前缀编码的二进制
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        match self {
            RegistryMessage::Register {
                serial,
                peer_id,
                pubkey,
                signature,
            } => {
                buf.put_u8(MSG_REGISTER);
                write_len_prefixed(&mut buf, serial.as_bytes());
                write_len_prefixed(&mut buf, peer_id);
                write_len_prefixed(&mut buf, pubkey);
                write_len_prefixed(&mut buf, signature);
            }
            RegistryMessage::Query { serial } => {
                buf.put_u8(MSG_QUERY);
                write_len_prefixed(&mut buf, serial.as_bytes());
            }
            RegistryMessage::Response {
                peer_id,
                pubkey,
                signature,
            } => {
                buf.put_u8(MSG_RESPONSE);
                write_len_prefixed(&mut buf, peer_id);
                write_len_prefixed(&mut buf, pubkey);
                write_len_prefixed(&mut buf, signature);
            }
            RegistryMessage::NotFound => {
                buf.put_u8(MSG_NOT_FOUND);
            }
            RegistryMessage::Error { message } => {
                buf.put_u8(MSG_ERROR);
                write_len_prefixed(&mut buf, message.as_bytes());
            }
        }
        buf.to_vec()
    }

    /// 从二进制反序列化 (要求整段即为一条完整消息)
    pub fn decode(data: &[u8]) -> Result<RegistryMessage> {
        let mut buf: &[u8] = data;
        if buf.is_empty() {
            return Err(anyhow!("registry: empty message"));
        }
        let tag = buf.get_u8();
        match tag {
            MSG_REGISTER => {
                let serial = String::from_utf8(read_len_prefixed(&mut buf)?)
                    .map_err(|e| anyhow!("registry: invalid serial utf8: {e}"))?;
                let peer_id = read_len_prefixed(&mut buf)?;
                let pubkey = read_len_prefixed(&mut buf)?;
                let signature = read_len_prefixed(&mut buf)?;
                Ok(RegistryMessage::Register {
                    serial,
                    peer_id,
                    pubkey,
                    signature,
                })
            }
            MSG_QUERY => {
                let serial = String::from_utf8(read_len_prefixed(&mut buf)?)
                    .map_err(|e| anyhow!("registry: invalid serial utf8: {e}"))?;
                Ok(RegistryMessage::Query { serial })
            }
            MSG_RESPONSE => {
                let peer_id = read_len_prefixed(&mut buf)?;
                let pubkey = read_len_prefixed(&mut buf)?;
                let signature = read_len_prefixed(&mut buf)?;
                Ok(RegistryMessage::Response {
                    peer_id,
                    pubkey,
                    signature,
                })
            }
            MSG_NOT_FOUND => Ok(RegistryMessage::NotFound),
            MSG_ERROR => {
                let message = String::from_utf8(read_len_prefixed(&mut buf)?)
                    .map_err(|e| anyhow!("registry: invalid error utf8: {e}"))?;
                Ok(RegistryMessage::Error { message })
            }
            other => Err(anyhow!("registry: unknown message tag {other}")),
        }
    }

    /// 计算签名原文: `serial_bytes || peer_id_bytes`
    pub fn sign_payload(serial: &str, peer_id: &[u8]) -> Vec<u8> {
        let mut v = serial.as_bytes().to_vec();
        v.extend_from_slice(peer_id);
        v
    }
}
