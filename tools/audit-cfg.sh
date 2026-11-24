#!/usr/bin/env bash
#
# ⚠️ 重要：本脚本的 HIGH 发现**需要编译器复核**，不能直接当判据。
#
# # 为什么（第四十轮实测）
#
# 脚本用 awk 逐行解析，试图静态推断「哪些符号在 cfg(windows) 下定义、
# 哪些引用在门控之外」。这种行级分析对下面这种跨多行的块形式**必然误报**：
#
# ```rust
# #[cfg(windows)]
# let (a, b, c) = {        // 名字列表在这行
#     let x = ...;         // 右值块跨多行
#     (1, 2, 3)
# };
# ```
#
# 实测：修复 cfg 配对后 `cargo check --target x86_64-unknown-linux-gnu`
# 是 **0 error**，而本脚本仍报 **12 条 HIGH**。
#
# # 正确的用法
#
# 1. 用本脚本**快速扫**可疑点（几秒，不需要 target）
# 2. 用 `tools/verify-cross-platform.ps1`（真 `cargo check`）**确认**
# 3. 两者冲突时，**编译器永远是对的**
#
# 编译器是唯一权威 —— 它做完整的名字解析与 cfg 展开，本脚本只是启发式。
# =============================================================================
# tools/audit-cfg.sh — SideLens 跨平台 cfg 门控审计
# =============================================================================
# 静态扫描 crates/**/*.rs 与 apps/**/*.rs，找出「用了 Windows-only API /
# 绑定了 Windows-only 符号，却没写 #[cfg(windows)] 门控」的地方。
#
# 为什么需要它：
#   cargo check 只能告诉你「当前 target 编不过」，不能告诉你**为什么**，也不能
#   在 Windows 上提前发现「这里将来移植会炸」。本脚本把「该门控没门控」当成
#   一类独立的静态缺陷来找，不依赖是否装了交叉 target。
#
# 用法：
#   tools/audit-cfg.sh                  # 从任意目录跑都行
#   tools/audit-cfg.sh --quiet          # 只打问题行
#   tools/audit-cfg.sh --strict         # 有 HIGH 则 exit 1（CI 用）
#   tools/audit-cfg.sh --root <dir>     # 指定仓库根
#
# 退出码：0 = 无 HIGH；1 = 有 HIGH（仅 --strict）；2 = 参数/环境错误
#
# 依赖：bash 3.2+、POSIX awk（WSL / git-bash / Linux / macOS 均可）。
#       刻意不用 python，避免「审计脚本本身跑不起来」。
# =============================================================================

set -uo pipefail

ROOT=""
QUIET=0
STRICT=0

while [ $# -gt 0 ]; do
    case "$1" in
        --root)   ROOT="${2:-}"; shift 2 ;;
        --root=*) ROOT="${1#--root=}"; shift ;;
        --quiet|-q) QUIET=1; shift ;;
        --strict) STRICT=1; shift ;;
        -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "audit-cfg: 未知参数 $1" >&2; exit 2 ;;
    esac
done

if [ -z "$ROOT" ]; then
    ROOT=$(cd "$(dirname "$0")/.." && pwd)
fi
[ -d "$ROOT/crates" ] || { echo "audit-cfg: $ROOT 下没有 crates/，用 --root 指定仓库根" >&2; exit 2; }

cd "$ROOT" || exit 2

TMPD=$(mktemp -d 2>/dev/null || mktemp -d -t auditcfg)
trap 'rm -rf "$TMPD"' EXIT

FILES="$TMPD/files.txt"
find crates apps -type f -name '*.rs' 2>/dev/null | LC_ALL=C sort > "$FILES"
NFILES=$(wc -l < "$FILES" | tr -d ' ')

mkdir -p "$TMPD/stripped"

strip_comments() {
    awk '
    BEGIN { in_block = 0 }
    {
        line = $0; out = ""; i = 1; n = length(line)
        if (in_block) {
            p = index(line, "*/")
            if (p == 0) { print ""; next }
            in_block = 0
            line = substr(line, p + 2); n = length(line); i = 1
        }
        while (i <= n) {
            c = substr(line, i, 1); two = substr(line, i, 2)
            if (two == "/*") {
                in_block = 1
                rest = substr(line, i + 2)
                p = index(rest, "*/")
                if (p == 0) break
                in_block = 0
                line = substr(rest, p + 2); i = 1; n = length(line)
                continue
            }
            if (two == "//") {
                prev = (i > 1) ? substr(line, i - 1, 1) : ""
                if (prev != ":") break
            }
            out = out c; i++
        }
        print out
    }
    ' "$1"
}

while IFS= read -r f; do
    strip_comments "$f" > "$TMPD/stripped/$(printf '%s' "$f" | tr '/' '_')"
done < "$FILES"

flat_of() { printf '%s' "$1" | tr '/' '_'; }

DEPIDX="$TMPD/deps.tsv"
: > "$DEPIDX"

find crates apps -name Cargo.toml -type f 2>/dev/null | LC_ALL=C sort | while IFS= read -r t; do
    crate=$(dirname "$t")
    awk -v crate="$crate" '
    BEGIN { inblk = 0 }
    /^[ \t]*\[/ {
        inblk = 0
        if ($0 ~ /\[target\..*cfg\(windows\).*\.(dev-)?dependencies\]/) inblk = 1
        next
    }
    inblk && /^[ \t]*[A-Za-z0-9_-]+[ \t]*=/ {
        name = $0
        sub(/^[ \t]*/, "", name); sub(/[ \t]*=.*$/, "", name)
        printf "%s\t%s\n", crate, name
    }
    ' "$t"
done >> "$DEPIDX"

is_mod_gated() {
    local target="$1" flat rel modpath child parent declfile dflat
    flat=$(flat_of "$target")

    if head -n 40 "$TMPD/stripped/$flat" | grep -Eq '^[[:space:]]*#!\[cfg[[:space:]]*\(.*windows'; then
        echo 1; return
    fi
    case "$target" in
        */examples/*|*/tests/*|*/benches/*) echo 0; return ;;
    esac

    rel=${target#*/src/}
    rel=${rel%.rs}
    case "$rel" in
        lib|main) echo 0; return ;;
        */mod) modpath=${rel%/mod} ;;
        *)     modpath=$rel ;;
    esac

    child=$(basename "$modpath")
    parent=$(dirname "$modpath")
    [ "$parent" = "." ] && parent=""

    if [ -z "$parent" ]; then
        declfile=$(dirname "$target")/lib.rs
        [ -f "$declfile" ] || declfile=$(dirname "$target")/main.rs
    else
        declfile=$(dirname "$target")/../${parent}.rs
        [ -f "$declfile" ] || declfile=$(dirname "$target")/../${parent}/mod.rs
    fi

    [ -f "$declfile" ] || { echo 0; return; }
    dflat=$(flat_of "$declfile")
    [ -f "$TMPD/stripped/$dflat" ] || { echo 0; return; }

    awk -v want="$child" '
    BEGIN { pend = ""; hit = 0 }
    {
        line = $0
        if (line ~ /^[ \t]*#\[/) { pend = pend " " line; next }
        if (pend != "" && line ~ /^[ \t]*\]/) { pend = pend " " line; next }
        if (pend != "" && line ~ /^[ \t]*$/) next
        if (line ~ /^[ \t]*(pub[ \t]+)?mod[ \t]+[A-Za-z0-9_]+[ \t]*;/) {
            name = line
            sub(/^[ \t]*(pub[ \t]+)?mod[ \t]+/, "", name)
            sub(/[ \t]*;.*$/, "", name)
            if (name == want && pend ~ /cfg[ \t]*\(/ && pend !~ /not[ \t]*\([ \t]*windows/) hit = 1
            pend = ""; next
        }
        if (line ~ /^[ \t]*(pub[ \t]+)?(fn|struct|enum|impl|use|const|static|mod)[ \t(]/) pend = ""
    }
    END { print hit }
    ' "$TMPD/stripped/$dflat"
}

FINDINGS="$TMPD/findings.tsv"
: > "$FINDINGS"

scan_file() {
    # $1 = original path (for reporting), $2 = whether the file is module-gated.
    # The awk input is the comment-stripped copy, otherwise `windows::`
    # mentioned inside doc comments is reported as a real usage (false positive).
    awk -v file="$1" -v file_gated="$2" -v out="$FINDINGS" '
    function ident_of(l,   t) {
        t = l
        sub(/^[ \t]*/, "", t)
        sub(/^pub(\([a-z]+\))?[ \t]+/, "", t)
        sub(/^unsafe[ \t]+/, "", t)
        sub(/^async[ \t]+/, "", t)
        sub(/^extern[ \t]+"[^"]*"[ \t]+/, "", t)
        if (t ~ /^let[ \t]/) {
            sub(/^let[ \t]+/, "", t)
            sub(/^mut[ \t]+/, "", t)
            if (t ~ /^\(/) {
                # 元组解构：`let (a, b, c) = ...` 或 `let (a, b) = { ... }`
                #
                # 第四十轮修：原来用 `sub(/\).*$/, "", t)` 截到**第一个** `)`，
                # 这对 `let (a, b)` 没问题，但如果名字后面还有嵌套括号就会截错。
                # 更关键的是：`let (a,b) = {` 这种「块形式」的右值里有 `{`，
                # 旧逻辑会把 `{` 之前的内容当名字，导致 `a b` 后面拖着 `= `。
                #
                # 现在改成按括号配对找结束位置。
                depth2 = 0; endp = 0
                for (q = 1; q <= length(t); q++) {
                    ch = substr(t, q, 1)
                    if (ch == "(") depth2++
                    else if (ch == ")") { depth2--; if (depth2 == 0) { endp = q; break } }
                }
                if (endp > 0) t = substr(t, 2, endp - 2)
                else t = substr(t, 2)
                gsub(/,/, " ", t)
                gsub(/[ \t]+/, " ", t)
                return t
            }
            sub(/[ \t]*[:=;].*$/, "", t)
            return t
        }
        if (t ~ /^(fn|struct|enum|trait|const|static|type|union)[ \t(]/) {
            sub(/^(fn|struct|enum|trait|const|static|type|union)[ \t(]+/, "", t)
            sub(/[ \t(<:;={].*$/, "", t)
            return t
        }
        if (t ~ /^mod[ \t]/ || t ~ /^use[ \t]/) return ""
        return ""
    }

    BEGIN {
        depth = 0; pending = 0; notpending = 0; curline = 0; nl = 0
        for (i = 0; i < 256; i++) gated[i] = 0
    }
    {
        nl++
        lines[nl] = $0
        eff_of[nl] = 0
        line = $0; ln = FNR

        if (line ~ /^[ \t]*#\[/ || line ~ /^[ \t]*#!\[/) {
            eff_of[nl] = 1
            if (line ~ /cfg[ \t]*\(/ && line ~ /windows/) {
                if (line ~ /not[ \t]*\([ \t]*windows/) { notpending = 1; pending = 0 }
                else if (line ~ /any[ \t]*\(/ && line ~ /target_os|target_arch|target_family|feature[ \t]*=|unix/) {
                    pending = 0
                    printf "%s\t%d\tCFG_ANY_MIXED\tHIGH\t%s\n", file, ln, "cfg(any(windows,...)) 混了非 windows 谓词，windows 分支在其他平台仍会编译" >> out
                } else pending = 1
            }
            curline = ln
            next
        }
        if (line ~ /^[ \t]*\]/ || line ~ /^[ \t]*$/) { eff_of[nl] = 1; next }

        eff = file_gated
        for (i = 1; i <= depth; i++) if (gated[i]) eff = 1
        if (pending) eff = 1
        eff_of[nl] = eff

        if (!eff) {
            if (line ~ /(^|[^A-Za-z0-9_])windows::/) {
                printf "%s\t%d\tWINDOWS_API_UNGATED\tHIGH\t%s\n", file, ln, "用了 windows:: 但所在模块/项没有 #[cfg(windows)]" >> out
            } else if (line ~ /(^|[^A-Za-z0-9_])windows_core::/) {
                printf "%s\t%d\tWINDOWS_API_UNGATED\tHIGH\t%s\n", file, ln, "用了 windows_core:: 但未门控" >> out
            } else if (line ~ /(^|[^A-Za-z0-9_])windows_sys::/) {
                printf "%s\t%d\tWINDOWS_API_UNGATED\tHIGH\t%s\n", file, ln, "用了 windows_sys:: 但未门控" >> out
            } else if (line ~ /(^|[^A-Za-z0-9_])winapi::/) {
                printf "%s\t%d\tWINDOWS_API_UNGATED\tHIGH\t%s\n", file, ln, "用了 winapi:: 但未门控" >> out
            } else if (line ~ /std::os::windows/) {
                printf "%s\t%d\tWINDOWS_API_UNGATED\tHIGH\t%s\n", file, ln, "用了 std::os::windows 但未门控" >> out
            }
        }

        # 关键：先把 pending 存下来再清空。
        # cfg 配对索引会用掉 pending，但同一行的 "{" 仍然需要它来标记
        # 「这个块是 cfg(windows) 打开的」。早期版本在这里丢掉 pending，
        # 导致 timer.rs / detect.rs 里的 fn 体被误判成未门控。
        # 关键：先把 pending 存下来再清空。
        #
        # # 为什么要把 id 拆成多个（第四十轮修）
        #
        # `ident_of` 对 `let (a, b, c) = ...` 返回的是空格分隔的 `"a b c"`，
        # 而 `win[id]` 是按**单名**索引的。直接 `win["a b c"]` 会让后面
        # 按单名 `a` 查找时全部落空 → 全部误报成 WINDOWS_SYMBOL_LEAK。
        #
        # 实测：修这个之前，`udp_server.rs` 报 12 条 HIGH，
        # 而 `cargo check --target x86_64-unknown-linux-gnu` 是 **0 error**。
        pbrace = pending
        if (pending || notpending) {
            id = ident_of(line)
            if (id != "") {
                nid = split(id, ids, " ")
                for (q = 1; q <= nid; q++) {
                    nm = ids[q]
                    if (nm == "") continue
                    if (pending)    { if (!(nm in win)) win[nm] = curline }
                    if (notpending) { if (!(nm in notw)) notw[nm] = curline }
                }
            }
            pending = 0; notpending = 0
        }

        n = length(line)
        for (i = 1; i <= n; i++) {
            c = substr(line, i, 1)
            if (c == "{") { depth++; gated[depth] = pbrace; pbrace = 0 }
            else if (c == "}") { gated[depth] = 0; if (depth > 0) depth--; pbrace = 0 }
            else if (c == ";") pbrace = 0
            else if (c == "," && depth > 0) pbrace = 0
        }
    }
    END {
        for (s in win) {
            if (s in notw) continue
            if (length(s) < 4) continue
            for (k = 1; k <= nl; k++) {
                if (eff_of[k]) continue
                if (k == win[s]) continue
                if (lines[k] ~ ("(^|[^A-Za-z0-9_])" s "([^A-Za-z0-9_]|$)")) {
                    printf "%s\t%d\tWINDOWS_SYMBOL_LEAK\tHIGH\t%s\n", file, k, "符号 " s " 只在 #[cfg(windows)] 下定义（第 " win[s] " 行），此处无门控引用，非 Windows 构建 E0425" >> out
                }
            }
        }
        for (id in win) {
            if (id in notw) continue
            printf "%s\t%d\tCFG_UNPAIRED\tLOW\t%s\n", file, win[id], "单边 #[cfg(windows)] 项 " id "，无 not(windows) 兄弟" >> out
        }
        for (id in notw) {
            if (id in win) continue
            printf "%s\t%d\tCFG_NOT_WINDOWS_ONLY\tLOW\t%s\n", file, notw[id], "单边 #[cfg(not(windows))] 项 " id >> out
        }
    }
    ' "$TMPD/stripped/$(flat_of "$1")"
}

while IFS= read -r f; do
    flat=$(flat_of "$f")
    src="$TMPD/stripped/$flat"
    [ -f "$src" ] || continue
    scan_file "$f" "$(is_mod_gated "$f")"
done < "$FILES"

if [ -s "$DEPIDX" ]; then
    sort -u "$DEPIDX" -o "$DEPIDX"
    while IFS=$'\t' read -r crate dep; do
        [ -n "$dep" ] || continue
        mod=${dep//-/_}
        case "$mod" in windows|windows_core|windows_sys) continue ;; esac
        while IFS= read -r f; do
            case "$f" in "$crate"/*) ;; *) continue ;; esac
            flat=$(flat_of "$f")
            src="$TMPD/stripped/$flat"
            [ -f "$src" ] || continue
            [ "$(is_mod_gated "$f")" = "1" ] && continue
            awk -v file="$f" -v mod="$mod" -v out="$FINDINGS" '
            BEGIN { depth = 0; pending = 0; for (i = 0; i < 256; i++) gated[i] = 0 }
            {
                line = $0; ln = FNR
                if (line ~ /^[ \t]*#\[/ || line ~ /^[ \t]*#!\[/) {
                    if (line ~ /cfg[ \t]*\(/ && line ~ /windows/ && line !~ /not[ \t]*\([ \t]*windows/) pending = 1
                    next
                }
                if (line ~ /^[ \t]*\]/ || line ~ /^[ \t]*$/) next
                eff = pending
                for (i = 1; i <= depth; i++) if (gated[i]) eff = 1
                pat = "(^|[^A-Za-z0-9_])" mod "::"
                if (!eff && line ~ pat) {
                    printf "%s\t%d\tTARGET_GATED_DEP_UNGATED\tHIGH\t%s\n", file, ln, "依赖 " mod " 只在 cfg(windows) 下声明，此处无门控引用" >> out
                }
                n = length(line)
                for (i = 1; i <= n; i++) {
                    c = substr(line, i, 1)
                    if (c == "{") { depth++; gated[depth] = pending; pending = 0 }
                    else if (c == "}") { gated[depth] = 0; if (depth > 0) depth--; pending = 0 }
                    else if (c == ";") pending = 0
                    else if (c == "," && depth > 0) pending = 0
                }
            }
            ' "$src"
        done < "$FILES"
    done < "$DEPIDX"
fi

sort -u "$FINDINGS" -o "$FINDINGS"

cnt() { awk -F'\t' -v s="$1" '$4==s' "$FINDINGS" | wc -l | tr -d ' '; }
HIGH=$(cnt HIGH); MED=$(cnt MEDIUM); LOW=$(cnt LOW); INF=$(cnt INFO)

if [ "$QUIET" = "0" ]; then
    echo "=============================================================="
    echo " SideLens 跨平台 cfg 门控审计"
    echo "=============================================================="
    echo " 仓库根   : $ROOT"
    echo " 扫描文件 : $NFILES 个 .rs"
    echo " HIGH=$HIGH  MEDIUM=$MED  LOW=$LOW  INFO=$INF"
    echo "--------------------------------------------------------------"
fi

if [ ! -s "$FINDINGS" ]; then
    echo " 未发现问题。"
else
    awk -F'\t' '$4!="LOW" { printf "%-6s %s:%s\n         %s - %s\n", $4, $1, $2, $3, $5 }' "$FINDINGS"
    if [ "$QUIET" = "0" ] && [ "$LOW" != "0" ]; then
        echo "--------------------------------------------------------------"
        echo " LOW（单边 cfg，信息性）共 $LOW 条："
        awk -F'\t' '$4=="LOW" { printf "   %s:%s  %s\n", $1, $2, $3 }' "$FINDINGS"
    fi
fi

if [ "$QUIET" = "0" ]; then
    echo "--------------------------------------------------------------"
    echo " 类型说明："
    echo "   WINDOWS_API_UNGATED      HIGH   用了 windows:: 等但无 #[cfg(windows)]"
    echo "   WINDOWS_SYMBOL_LEAK      HIGH   cfg(windows) 专属符号在未门控区域被引用"
    echo "   TARGET_GATED_DEP_UNGATED HIGH   依赖只在 cfg(windows) 下声明，此处裸用"
    echo "   CFG_ANY_MIXED            HIGH   cfg(any(windows, ...)) 混了别的谓词"
    echo "   CFG_UNPAIRED             LOW    单边 #[cfg(windows)]"
    echo "   CFG_NOT_WINDOWS_ONLY     LOW    单边 #[cfg(not(windows))]"
    echo "=============================================================="
fi

if [ "$STRICT" = "1" ] && [ "$HIGH" != "0" ]; then
    exit 1
fi
exit 0