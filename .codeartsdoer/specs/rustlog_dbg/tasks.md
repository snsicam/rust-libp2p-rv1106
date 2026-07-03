# 编码任务清单

## 任务1：修改 run_relay.sh 的 RUST_LOG 设置

**文件**：`p2p-camera/scripts/run_relay.sh`

**修改内容**：
- 将第27行的内联 `RUST_LOG=info "$BIN" --port $PORT --key-file "$KEY_FILE"` 拆分为两行
- 添加 `export RUST_LOG="${RUST_LOG:-info,}libp2p_dcutr=debug,libp2p_relay=debug"`
- 将启动命令改为 `exec "$BIN" --port $PORT --key-file "$KEY_FILE"`

**验收条件**：
- 脚本执行后 RUST_LOG 包含 `libp2p_dcutr=debug,libp2p_relay=debug`
- 用户预设 RUST_LOG 时，原有值被保留并追加 dcutr/relay debug 配置
- 脚本参数接口不变

---

## 任务2：修改 run_device_cam.sh 的 RUST_LOG 设置

**文件**：`p2p-camera/scripts/run_device_cam.sh`

**修改内容**：
- 将第52行的 `export RUST_LOG="${RUST_LOG:-info}"` 替换为 `export RUST_LOG="${RUST_LOG:+$RUST_LOG,}libp2p_dcutr=debug,libp2p_relay=debug"`

**验收条件**：
- 脚本执行后 RUST_LOG 包含 `libp2p_dcutr=debug,libp2p_relay=debug`
- 用户预设 RUST_LOG 时，原有值被保留并追加 dcutr/relay debug 配置
- 脚本参数接口不变

---

## 任务3：修改 run_viewer.sh 的 RUST_LOG 设置

**文件**：`p2p-camera/scripts/run_viewer.sh`

**修改内容**：
- 将第58行的 `export RUST_LOG="${RUST_LOG:-info}"` 替换为 `export RUST_LOG="${RUST_LOG:+$RUST_LOG,}libp2p_dcutr=debug,libp2p_relay=debug"`

**验收条件**：
- 脚本执行后 RUST_LOG 包含 `libp2p_dcutr=debug,libp2p_relay=debug`
- 用户预设 RUST_LOG 时，原有值被保留并追加 dcutr/relay debug 配置
- 脚本参数接口不变
