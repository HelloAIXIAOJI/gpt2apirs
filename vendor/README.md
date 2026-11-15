# vendor/

第三方源码快照，作为 path 依赖使用，不参与本仓库的 lint 与注释规范。

| 目录 | 上游 | 锁定版本/提交 | 许可 |
|---|---|---|---|
| `shoes/` | https://github.com/cfal/shoes | v0.3.7 (cf27c458419b) | MIT |
| `rustls/` | https://github.com/cfal/rustls | f1a23c187330 | Apache-2.0 / ISC / MIT |
| `blake3/` | https://github.com/cfal/BLAKE3 | cd98c1c4112e | CC0-1.0 / Apache-2.0 |
| `h3/` | https://github.com/cfal/h3 | 90e3013710e3 | MIT |
| `hickory/` | https://github.com/cfal/hickory-dns | 93435e55f102 | MIT / Apache-2.0 |
| `nix/` | https://github.com/nix-rust/nix | 3eae6d68ec63 | MIT |
| `smoltcp/` | https://github.com/cfal/smoltcp | 7ef43e497f57 | 0BSD |
| `gotatun/` | https://github.com/mullvad/gotatun | c06582e41022 | MIT |

## 我们对第三方源码做过的改动

- `shoes/Cargo.toml`：删掉内嵌的 `[workspace]` / `[patch.crates-io]` / `[[test]]` 等块，
  因为 workspace 与补丁声明只能出现在本仓库根部；同时把 `*` 版本号全部固定成上游
  `Cargo.lock` 里的实际版本（不固定会跟着本仓库的锁漂移，例如 `rand` 会被解析成 0.9，
  而上游代码用的是 0.10 的 API）。

### 为嵌入做的补丁

- `shoes/src/cli.rs`：把 `run()` 拆成 `run(args)` 与 `run_args(...)`，新增公开的
  `run_config(path, threads)`；日志初始化失败不再 `return 1`（宿主可能已装 logger）。
- `shoes/src/process.rs`：新增 `request_shutdown()` / `reset_shutdown()`，并让
  `ShutdownSignals::recv` 在收到该请求时返回，从而支持进程内停机（Windows 没有 SIGTERM）。
- `shoes/src/lib.rs`：`mod process` 改为 `pub mod process`。
- `shoes/Cargo.toml`：`crate-type` 收敛为 `["rlib"]`，本仓库只把它当库用；
  保留 cdylib/staticlib 会让链接阶段报「required to be available in rlib format」。
- 所有第三方 crate 的 `src/lib.rs` 顶部加 `#![allow(warnings)]`：这些代码不参与本仓库的
  零告警门禁，否则 `RUSTFLAGS=-D warnings` 会把它们的死代码告警变成构建失败。

## 构建注意

- Windows 上主线程默认栈 1MB，`shoes` 启动会栈溢出；二进制需 `/STACK:16777216`，
  以库形式嵌入时必须放在大栈线程里运行（`crates/proxy-core` 用 16MB）。
