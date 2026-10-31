#!/usr/bin/env bash
# scripts/test-affected.sh — DRAFT 供业主评审(refactor/gate-hierarchy)
#
# 受影响目标选择(gate-hierarchy-2026-10.md §3):读 git diff --name-only(相对
# BASE),经 scripts/test-affected-mapping.yaml 映射为具体测试命令(含后端与平台
# 环境),去重后逐条执行。
#
# 用法:
#   scripts/test-affected.sh [--dry-run] [BASE_REF]
#     BASE_REF 缺省 = git merge-base HEAD origin/main;任务卡工作流传 baseline 提交。
#   环境变量:
#     MINISS_AFFECTED_BASE          优先于位置参数
#     MINISS_AFFECTED_ALLOW_EMPTY=1 允许空选择继续(默认拒绝——绝不静默为空)
#
# 退出码:0 = 选中命令全部通过(或 dry-run);1 = 有选中命令失败;
# 2 = 拒绝(空选择、BASE 无法解析、映射文件缺失/无兜底规则)。
#
# 平台/后端可用性镜像 build.rs:67-91:io_uring/epoll 仅 Linux;kqueue 仅 macOS。
# 兼容性:bash 3.2(macOS /bin/bash;无 mapfile、无关联数组、heredoc 不进 $()——
# 与 audit_architecture.sh 同一规避),python3(仓库既有依赖),git。

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
MAP="${MINISS_AFFECTED_MAP:-$ROOT/scripts/test-affected-mapping.yaml}"
DRY_RUN=0
BASE="${MINISS_AFFECTED_BASE:-}"
BASE_ARG=""

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) BASE_ARG="$arg" ;;
    esac
done
if [ -n "$BASE_ARG" ]; then BASE="$BASE_ARG"; fi

if [ ! -f "$MAP" ]; then
    echo "REFUSAL: mapping file missing: $MAP" >&2
    exit 2
fi

if [ -z "$BASE" ]; then
    BASE="$(git merge-base HEAD origin/main 2>/dev/null || true)"
fi
if [ -z "$BASE" ]; then
    echo "REFUSAL: no BASE given and 'origin/main' is unavailable." >&2
    echo "  Pass the task card's baseline: scripts/test-affected.sh <baseline-commit>" >&2
    exit 2
fi
BASE="$(git rev-parse --verify --quiet "$BASE^{commit}" 2>/dev/null || true)"
if [ -z "$BASE" ]; then
    echo "REFUSAL: BASE '${BASE_ARG:-MINISS_AFFECTED_BASE}' does not resolve to a commit." >&2
    exit 2
fi

CHANGED="$(git diff --name-only "$BASE" | sort -u)"
N_CHANGED="$(printf '%s\n' "$CHANGED" | grep -c . || true)"
if [ "$N_CHANGED" -eq 0 ]; then
    echo "NO-OP: no changed files vs $BASE — nothing to select (not a pass, not a fail)." >&2
    exit 2
fi

case "$(uname -s)" in
    Linux) PLATFORM=linux ;;
    Darwin) PLATFORM=macos ;;
    *) PLATFORM=other ;;
esac

# bash 3.2:heredoc 不能放进 $(),且 heredoc 会占用 stdin——匹配器先写到临时文件,
# 变更文件清单经管道喂给它(audit_architecture.sh 对 $()+heredoc 的同一规避)。
TMP_SEL="$(mktemp "${TMPDIR:-/tmp}/test-affected.XXXXXX")"
TMP_PY="$(mktemp "${TMPDIR:-/tmp}/test-affected.XXXXXX")"
cleanup() { rm -f "$TMP_SEL" "$TMP_PY"; }
trap cleanup EXIT INT TERM

# 解析映射、最长前缀匹配、跨文件去重;输出 TSV:
#   platform <TAB> backend <TAB> cmd <TAB> 首个触发文件
# 特殊行:UNMATCHED <TAB> - <TAB> <文件列表>(兜底规则使其恒空,保留作守卫)
cat >"$TMP_PY" <<'PY'
import re
import sys

map_path, host_platform = sys.argv[1], sys.argv[2]
files = [line.rstrip("\n") for line in sys.stdin if line.strip()]

text = open(map_path, encoding="utf-8").read()
lines = []
for raw in text.splitlines():
    if raw.lstrip().startswith("#"):
        continue
    lines.append(raw.split("#", 1)[0])   # 受约束子集:命令字符串不含 '#'

rules = []
rule = None
in_legs = False
leg_re = re.compile(
    r'-\s*\{\s*platform:\s*(\w+)\s*,\s*backend:\s*(\w+)\s*,\s*cmd:\s*"([^"]*)"\s*\}'
)
match_re = re.compile(r'-\s*match:\s*\[([^\]]*)\]')
for line in lines:
    s = line.strip()
    if not s:
        continue
    indent = len(line) - len(line.lstrip())
    if indent == 0:
        continue
    if indent <= 2:                      # 新 rule
        m = match_re.match(s)
        if m:
            items = [x.strip().strip('"\'') for x in m.group(1).split(",") if x.strip()]
            rule = {"match": items, "legs": []}
            rules.append(rule)
            in_legs = False
        continue
    if s.startswith("legs:"):
        in_legs = True
        continue
    if s.startswith("comment:"):
        continue
    if in_legs and rule is not None:
        m = leg_re.match(s)
        if m:
            rule["legs"].append(
                {"platform": m.group(1), "backend": m.group(2), "cmd": m.group(3)}
            )

if not any(r["match"] == [""] for r in rules):
    sys.stderr.write('REFUSAL: mapping has no "" fallback rule\n')
    sys.exit(2)

def prefix_len(path, prefix):
    if prefix == "":
        return 0
    if path == prefix:
        return len(prefix)
    if prefix.endswith("/"):
        return len(prefix) if path.startswith(prefix) else -1
    if path.startswith(prefix + "/"):
        return len(prefix)
    return -1

selected, matched = [], set()
for path in files:
    best, best_len = None, -1
    for rule in rules:
        for prefix in rule["match"]:
            n = prefix_len(path, prefix)
            if n > best_len:
                best, best_len = rule, n
    if best is not None and best_len >= 0:
        matched.add(path)
        for leg in best["legs"]:
            selected.append((leg["platform"], leg["backend"], leg["cmd"], path))

unmatched = [f for f in files if f not in matched]
seen, out = set(), []
for platform, backend, cmd, src in selected:
    key = (platform, backend, cmd)
    if key in seen:
        continue
    seen.add(key)
    out.append(f"{platform}\t{backend}\t{cmd}\t{src}")

for line in out:
    print(line)
if unmatched:
    print("UNMATCHED\t-\t" + " ".join(unmatched) + "\t-")
if not out:
    sys.exit(2)
PY

set +e
printf '%s\n' "$CHANGED" | python3 "$TMP_PY" "$MAP" "$PLATFORM" >"$TMP_SEL"
RC_SEL=$?
set -e

if [ "$RC_SEL" -ne 0 ] && [ ! -s "$TMP_SEL" ]; then
    echo "REFUSAL: affected-target selection is EMPTY for $N_CHANGED changed file(s) vs $BASE." >&2
    echo "  Empty is never silent: add a mapping rule for these paths or fix the mapping file." >&2
    if [ "${MINISS_AFFECTED_ALLOW_EMPTY:-0}" != "1" ]; then
        exit 2
    fi
    echo "  MINISS_AFFECTED_ALLOW_EMPTY=1: continuing with an empty selection." >&2
    exit 0
fi
if [ "$RC_SEL" -ne 0 ]; then
    echo "REFUSAL: mapping selection exited $RC_SEL (see python diagnostics above)." >&2
    exit 2
fi

echo "=== affected-target selection: base=$BASE platform=$PLATFORM files=$N_CHANGED ==="

RC=0
TOTAL=0
FAILED=""
DEDUP="|"

run_one() {
    label="$1"
    cmd="$2"
    key="|$label|$cmd|"
    case "$DEDUP" in
        *"$key"*) return 0 ;;
    esac
    DEDUP="$key$DEDUP"
    TOTAL=$((TOTAL + 1))
    if [ "$DRY_RUN" = "1" ]; then
        echo "[dry-run] [$label] $cmd"
        return 0
    fi
    echo "--- [$label] $cmd"
    if eval "$cmd"; then
        return 0
    fi
    echo "AFFECTED-FAIL: [$label] $cmd" >&2
    FAILED="$FAILED
  [$label] $cmd"
    RC=1
    return 0    # 跑完全部腿再汇总,失败清单一次给全
}

run_backend() {
    leg_backend="$1"
    leg_cmd="$2"
    case "$leg_backend" in
        host)
            run_one "host" "$leg_cmd"
            ;;
        io_uring)
            if [ "$PLATFORM" != "linux" ]; then
                echo "SKIP: io_uring leg not applicable on $PLATFORM" >&2
                return 0
            fi
            MINISS_IO_BACKEND=io_uring run_one "io_uring" "$leg_cmd"
            ;;
        epoll)
            if [ "$PLATFORM" != "linux" ]; then
                echo "SKIP: epoll leg not applicable on $PLATFORM" >&2
                return 0
            fi
            MINISS_IO_BACKEND=epoll CARGO_TARGET_DIR=target/epoll run_one "epoll" "$leg_cmd"
            ;;
        kqueue)
            if [ "$PLATFORM" != "macos" ]; then
                echo "SKIP: kqueue leg not applicable on $PLATFORM" >&2
                return 0
            fi
            MINISS_IO_BACKEND=kqueue run_one "kqueue" "$leg_cmd"
            ;;
        both)
            if [ "$PLATFORM" = "linux" ]; then
                MINISS_IO_BACKEND=io_uring run_one "io_uring" "$leg_cmd"
                MINISS_IO_BACKEND=epoll CARGO_TARGET_DIR=target/epoll run_one "epoll" "$leg_cmd"
            elif [ "$PLATFORM" = "macos" ]; then
                MINISS_IO_BACKEND=kqueue run_one "kqueue" "$leg_cmd"
            else
                echo "SKIP: both-backend leg not applicable on $PLATFORM" >&2
            fi
            ;;
        *)
            echo "REFUSAL: unknown backend '$leg_backend' in mapping" >&2
            exit 2
            ;;
    esac
}

while IFS=$'\t' read -r leg_platform leg_backend leg_cmd rest; do
    case "$leg_platform" in
        UNMATCHED)
            echo "WARNING: changed files matched no rule: $leg_cmd" >&2
            continue
            ;;
        any) ;;
        "$PLATFORM") ;;
        *) continue ;;
    esac
    [ -n "$leg_cmd" ] || continue
    run_backend "$leg_backend" "$leg_cmd"
done <"$TMP_SEL"

if [ "$DRY_RUN" = "1" ]; then
    echo "=== dry-run: $TOTAL command(s) selected; nothing executed ==="
    exit 0
fi

if [ "$RC" -ne 0 ]; then
    echo "=== affected-target selection FAILED ($TOTAL command(s) run; failures below) ===" >&2
    printf '%s\n' "$FAILED" >&2
    exit 1
fi
echo "=== affected-target selection GREEN ($TOTAL command(s)) ==="
exit 0
