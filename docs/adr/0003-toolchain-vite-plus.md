# ADR 0003：前端工具链统一用 Vite+（vp）

- 状态：已采纳
- 日期：2026-11-24

## 决策

本项目**只用 `vp`** 管理依赖与任务，不使用 `npm` / `pnpm` / `yarn` 直接调用。

| 场景 | 命令 |
| --- | --- |
| 安装依赖 | `vp install` |
| 添加依赖 | `vp add <pkg>` / `vp add -D <pkg>` |
| 移除依赖 | `vp remove <pkg>` |
| 跑测试 | `vp test run` / `vp test watch` |
| 跑脚本 | `vp run <script>` |
| 执行本地 bin | `vp exec <bin>` |
| 查看依赖树 | `vp ls` |
| 查看环境 | `vp env current` / `vp toolchain` |

## 迁移时踩的坑

### 1. workspace 内部依赖不能用语义化版本

原来 `crates`（`packages`）之间写的是 `"@sidelens/core": "0.1.0"`，
`vp install`（底层 pnpm）会把它当成**从 registry 下载的包**，于是 404：

```
ERR_PNPM_FETCH_404  GET https://registry.npmmirror.com/@sidelens%2Fsender: Not Found
```

必须改成 **workspace 协议**：

```json
{ "dependencies": { "@sidelens/core": "workspace:*" } }
```

### 2. workspaces 字段会被自动转成 pnpm-workspace.yaml

`vp install` 发现 `package.json` 里的 `workspaces` 字段时，会自动生成
`pnpm-workspace.yaml` 并提示：

```
[WARN] Created "pnpm-workspace.yaml" from the "workspaces" field in package.json.
```

此时应删掉 `package.json` 里的 `workspaces`，只保留 `pnpm-workspace.yaml`，
避免两处配置漂移。

### 3. node:test 的测试跑不起来

`vp test` 底层是 **Vitest**，不认识 `node:test` 的 `test()` 用法，
会报 `No test suite found in file`。全部测试已改写为 Vitest 风格：

```js
import { test, describe, expect } from "vitest";
```

### 4. `vp test` 是内建命令，不是 npm script

```
note: You are running `vp test` as a Vite+ built-in command.
      If you meant to run the test npm script, use `vpr test` instead.
```

要跑 `package.json` 里名为 `test` 的 script，用 `vpr test`。

## 产物

- `pnpm-lock.yaml` + `pnpm-workspace.yaml`（不再是 `package-lock.json`）
- 依赖被硬链接自内容寻址存储：`D:\.pnpm-store\v11`
