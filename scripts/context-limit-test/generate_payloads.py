#!/usr/bin/env python3
"""生成上下文边界测试 payload，并支持参数化运行。

测试矩阵:
  A. prompt 纯文本递增 → 找到 input_exceeds_limit 边界
  B. 多轮历史递增   → 找到 file upload 边界
  C. tools 开销测试  → 量化 tool 定义注入的 token 成本

配置: 在下方 TOKEN_POINTS 定义测试点。
"""
import json, os, sys

OUT_DIR = os.path.join(os.path.dirname(__file__), "payloads")

# ── 填充文本 ──────────────────────────────────
# 使用中英混合以模拟真实 Claude Code 场景
FILLER_ZH = "请仔细阅读以下背景资料。这段文本用于测试上下文窗口的实际容量上限。" * 50
FILLER_EN = "Please read the following background material carefully. This text is used to test the actual capacity limit of the context window. " * 50
FILLER = FILLER_ZH + FILLER_EN  # ~400 chars, ~200 tokens

def make_text(estimated_tokens: int) -> str:
    """生成约 estimated_tokens 个 token 的填充文本"""
    # 粗略: 2 chars ≈ 1.5 tokens for CJK, 4 chars ≈ 1 token for EN
    # 安全边距: ~2.2 chars/token 平均
    chars_needed = int(estimated_tokens * 2.2)
    repeats = chars_needed // len(FILLER) + 1
    return (FILLER * repeats)[:chars_needed]

def make_history(turns: int, tokens_per_turn: int = 500) -> list[dict]:
    """生成多轮历史: [user, assistant] * turns"""
    msgs = []
    for i in range(turns):
        msgs.append({"role": "user", "content": make_text(tokens_per_turn) + f" 第{i+1}轮"})
        msgs.append({"role": "assistant", "content": make_text(tokens_per_turn // 2) + f" 回复{i+1}"})
    return msgs

# ── 最小 tool 用于测试注入开销 ──────────────────
MINIMAL_TOOL = {
    "type": "function",
    "function": {
        "name": "echo",
        "description": "返回输入文本",
        "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
    }
}

# ── 测试点定义 ─────────────────────────────────
# 格式: (文件名前缀, 描述, request_dict)
# 每个测试点生成 .json 文件供 adapter_cli 使用

def gen_plan():
    tests = []

    # === A 系列: 单轮文本递增 ===
    # 无 tools，排除 tool injection 开销
    prompt_sizes = [1_000, 8_000, 16_000, 32_000, 48_000, 64_000, 80_000, 96_000, 128_000]
    for size in prompt_sizes:
        label = f"A_{size//1000:02d}K"
        tests.append((label, f"单轮 {size//1000}K tokens (纯文本, no tools)", {
            "model": "deepseek-default",
            "messages": [
                {"role": "user", "content": make_text(size) + "\n\n请用一句话总结以上内容。"}
            ],
            "stream": False
        }))

    # === B 系列: 多轮历史递增 ===
    # 无 tools，历史走文件上传
    turn_counts = [4, 16, 32, 64, 96, 128]
    for turns in turn_counts:
        total_tokens = turns * (500 + 250)  # user 500 + assistant 250 per turn
        label = f"B_{turns:03d}t"
        tests.append((label, f"{turns} 轮历史 (每轮 500t user + 250t asst = ~{total_tokens//1000}K total)", {
            "model": "deepseek-default",
            "messages": make_history(turns, 500) + [
                {"role": "user", "content": "请用一句话总结以上对话。"}
            ],
            "stream": False
        }))

    # === C 系列: tools 开销 ===
    # 分别测试 0/1/10/44 tools 下的实际 token 开销
    tool_counts = [0, 1, 10]
    for n in tool_counts:
        label = f"C_{n:02d}tools"
        req = {
            "model": "deepseek-default",
            "messages": [{"role": "user", "content": "你好"}],
            "stream": False
        }
        if n > 0:
            req["tools"] = [dict(MINIMAL_TOOL) for _ in range(n)]
            for i, t in enumerate(req["tools"]):
                t["function"] = dict(t["function"])
                t["function"]["name"] = f"echo_{i}"
        tests.append((label, f"{n} tools 开销测试", req))

    # === D 系列: 带 tools 的 prompt 递增 ===
    # 模拟真实 Claude Code 场景 (44 tools)
    tool_def = dict(MINIMAL_TOOL)
    for size in [1_000, 8_000, 16_000, 32_000, 48_000, 64_000]:
        label = f"D_{size//1000:02d}K_1tool"
        tests.append((label, f"带 1 tool + {size//1000}K prompt", {
            "model": "deepseek-default",
            "messages": [
                {"role": "user", "content": make_text(size) + "\n\n请用一句话总结以上内容。"}
            ],
            "tools": [dict(tool_def)],
            "stream": False
        }))

    return tests

def main():
    dry_run = "--dry-run" in sys.argv
    tests = gen_plan()
    os.makedirs(OUT_DIR, exist_ok=True)

    print(f"输出目录: {OUT_DIR}")
    print(f"测试用例数: {len(tests)}")
    print()

    if dry_run:
        for name, desc, _ in tests:
            print(f"  {name}: {desc}")
        print("\n执行: python3 generate_payloads.py")
        return

    for name, desc, req in tests:
        path = os.path.join(OUT_DIR, f"{name}.json")
        with open(path, 'w', encoding='utf-8') as f:
            json.dump(req, f, ensure_ascii=False, indent=2)
        body_str = json.dumps(req, ensure_ascii=False)
        kb = len(body_str.encode('utf-8')) / 1024
        print(f"  {name}.json  {kb:.0f}KB  {desc}")

    # 生成运行脚本
    script = os.path.join(os.path.dirname(__file__), "run_test.sh")
    with open(script, 'w') as f:
        f.write("#!/bin/bash\n")
        f.write("# DeepSeek 网页端上下文边界测试\n")
        f.write("# 用法: bash run_test.sh              # 运行全部\n")
        f.write("#       bash run_test.sh A_ 8 16      # 运行 A_08K ~ A_16K\n")
        f.write("#       bash run_test.sh --dry          # 列出测试项\n\n")
        f.write('FILTER="${1:-}"\n')
        f.write('shift 2>/dev/null || true\n')
        f.write('START="${1:-}"\n')
        f.write('END="${2:-}"\n')
        f.write('RESULTS="$(dirname "$0")/results.txt"\n')
        f.write('echo "=== 上下文边界测试 $(date) ===" > "$RESULTS_FILE"\n\n')
        f.write(f'cd "{os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))}"\n')
        f.write('echo "工作目录: $(pwd)"\n\n')

        for name, desc, _ in tests:
            f.write(f'if [ -z "$FILTER" ] || [[ "{name}" == "$FILTER"* ]]; then\n')
            f.write(f'  echo ""\n')
            f.write(f'  echo "========== [{name}] {desc} =========="\n')
            f.write(f'  echo "========== [{name}] {desc} ==========" >> "$RESULTS"\n')
            f.write(f'  cargo run --example adapter_cli -- -c config.toml -- chat payloads/{name}.json 2>&1 | tee -a "$RESULTS"\n')
            f.write(f'  EXIT_CODE=$?\n')
            f.write(f'  if grep -qiE "input_exceeds_limit|content is too long|ProviderError|BadRequest|biz_code" "$RESULTS"; then\n')
            f.write(f'    echo ">>> [{name}] 检测到限制! <<<"\n')
            f.write(f'  fi\n')
            f.write(f'fi\n')

        f.write('\necho ""\n')
        f.write('echo "测试完成 $(date)" | tee -a "$RESULTS"\n')

    os.chmod(script, 0o755)
    print(f"\n运行脚本: {script}")

if __name__ == "__main__":
    main()
