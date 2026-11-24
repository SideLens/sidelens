# ADR 0001：单仓 monorepo，暂不拆仓

- 状态：已采纳
- 日期：2026-11-24

## 背景

核心（协议 / 传输 / 时钟）与各平台 UI 在技术上可分离。曾考虑在 GitHub 组织下
为每个平台单独建仓。

## 决策

**先只建一个 monorepo**，`crates/`（或本 demo 的 `packages/`）与 `platforms/`（`apps/`）物理分开。

## 理由

| 维度 | 单仓 | 多仓 |
| --- | --- | --- |
| core / UI 的 ABI 版本 | 同一 commit 天然对齐 | 靠 tag 约束，漂移即失败 |
| 改一行协议 | 1 个 PR 全绿 | core PR + N 个 UI PR 连锁升级 |
| 移动端 so / 桌面 lib | 直接写入产物目录 | 需发布二进制包 + 版本引用 |
| CI | 一套 workflow，路径过滤 | 每仓一套 |

当前只有 2 个端（Windows + Android），多仓唯一优势（独立发布）收益最小。

## 拆仓触发条件

1. UI 数量 ≥ 4，单仓 CI 变长、clone 变大；
2. core 对外开放依赖，需要独立发 crates.io 包并走 semver；
3. 贡献者出现明显分工，PR review 互相干扰。

三条都不满足时，拆仓只会增加切仓、对版本、修 CI 的日常负担。
