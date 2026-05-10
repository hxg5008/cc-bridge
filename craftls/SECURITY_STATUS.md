# craftls 安全状态记录

## 当前版本
- 基于 `rustls 0.22.4` (2024-04-19) — **0.22 分支最后一个版本**

## 已包含的安全修复
- ✅ RUSTSEC-2024-0399 (网络可达 panic in `Acceptor::accept`, fragmented ClientHello)
- ✅ CVE-2024-32650 (`complete_io` 无限循环, close_notify after client_hello)

## 已知未跟进
- 上游 rustls 主线已在 0.23.x 演进。本 fork 停在 0.22.x,
  未来 0.23.x 出现的 CVE 不会自动 backport。
- 截至 2026-05 撰写时, **0.22.4 上无已知未修复 CVE**。

## 维护策略
1. 每季度对照 https://rustsec.org/advisories/ 检查 rustls 新增 advisory
2. 任何标记影响 0.22.x 或 "all versions" 的 advisory → 必须 backport
3. 升级到 rustls 0.23.x 是长期项, 风险:
   - 上游 API 变更, craft TLS 指纹层需要重写
   - ML-KEM / X25519MLKEM768 hybrid kx group 在 0.23.x 已 native 支持,
     可能不再需要本 fork
4. 关键参考: https://github.com/rustls/rustls/security
