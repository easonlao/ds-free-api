#!/usr/bin/env python3
"""上下文边界扫描 —— 二分查找 input_exceeds_limit 的确切边界。

用法:
  # 先启动服务
  just serve
  # 在另一个终端
  python3 scripts/context-limit-test/detect_boundary.py --mode prompt   # 测 prompt 限制
  python3 scripts/context-limit-test/detect_boundary.py --mode multiturn # 测多轮历史限制
  python3 scripts/context-limit-test/detect_boundary.py --mode tools     # 测 tools 开销

依赖: pip install httpx
"""
import argparse, json, math, sys, time
import httpx

API_BASE = "http://127.0.0.1:5317/v1"

FILLER_ZH = (
    "请仔细阅读以下背景资料。这段文本用于测试上下文窗口的实际容量上限。"
    "测试目标是找到 DeepSeek 网页内部 API 能够接受的最大输入 token 数量。"
    * 50
)
FILLER_EN = (
    "Please read the following background material carefully. "
    "This text is used to test the actual capacity limit of the context window. "
    "The goal is to find the maximum input tokens that DeepSeek web API accepts. "
    * 50
)
FILLER = FILLER_ZH + FILLER_EN  # ~500 chars

CHARS_PER_TOKEN = 2.2  # 混合中英文的粗略平均


def make_text(target_tokens: int) -> str:
    chars_needed = int(target_tokens * CHARS_PER_TOKEN)
    repeats = chars_needed // len(FILLER) + 1
    return (FILLER * repeats)[:chars_needed]


def make_request(prompt_tokens: int, history_turns: int = 0, tool_count: int = 0) -> dict:
    msgs = []
    if history_turns > 0:
        for i in range(history_turns):
            msgs.append({"role": "user", "content": make_text(100) + f" 第{i+1}轮提问"})
            msgs.append({"role": "assistant", "content": make_text(50) + f" 第{i+1}轮回复"})
    msgs.append({"role": "user", "content": make_text(prompt_tokens) + "\n\n请用一句话总结以上内容。"})

    req = {
        "model": "deepseek-default",
        "messages": msgs,
        "stream": False,
    }
    if tool_count > 0:
        req["tools"] = [
            {
                "type": "function",
                "function": {
                    "name": f"fn_{i}",
                    "description": f"测试工具 {i}",
                    "parameters": {"type": "object", "properties": {}, "required": []},
                },
            }
            for i in range(tool_count)
        ]
    return req


def test_single(tokens: int, history_turns: int, tool_count: int, client: httpx.Client) -> dict:
    """发送单次请求，返回结果摘要"""
    req = make_request(tokens, history_turns, tool_count)
    payload_bytes = json.dumps(req, ensure_ascii=False).encode()
    payload_kb = len(payload_bytes) / 1024

    start = time.monotonic()
    try:
        resp = client.post(
            f"{API_BASE}/chat/completions",
            json=req,
            timeout=120.0,
        )
        elapsed = time.monotonic() - start

        if resp.status_code != 200:
            body = resp.text[:300]
            return {"status": "HTTP_ERROR", "code": resp.status_code, "body": body,
                    "tokens": tokens, "payload_kb": payload_kb, "elapsed": elapsed}

        data = resp.json()
        usage = data.get("usage", {})
        prompt_tokens = usage.get("prompt_tokens", 0)
        completion_tokens = usage.get("completion_tokens", 0)
        content = ""
        if data.get("choices"):
            content = data["choices"][0].get("message", {}).get("content", "")[:100]

        return {
            "status": "OK",
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "content": content,
            "request_tokens": tokens,
            "payload_kb": payload_kb,
            "elapsed": elapsed,
        }

    except httpx.ReadTimeout:
        elapsed = time.monotonic() - start
        return {"status": "TIMEOUT", "tokens": tokens, "payload_kb": payload_kb, "elapsed": elapsed}
    except Exception as e:
        elapsed = time.monotonic() - start
        return {"status": "EXCEPTION", "error": str(e)[:200], "tokens": tokens,
                "payload_kb": payload_kb, "elapsed": elapsed}


def binary_search(low: int, high: int, history_turns: int, tool_count: int, client: httpx.Client):
    """二分查找 input_exceeds_limit 的边界"""
    print(f"\n=== 二分扫描: {low // 1000}K - {high // 1000}K tokens "
          f"(history_turns={history_turns}, tools={tool_count}) ===\n")

    last_ok = low
    last_fail = high

    while last_fail - last_ok > 1000:  # 1K token 精度
        mid = (last_ok + last_fail) // 2
        result = test_single(mid, history_turns, tool_count, client)
        status = result["status"]
        pt = result.get("prompt_tokens", 0)
        elapsed = result.get("elapsed", 0)

        if status == "OK":
            print(f"  ✅ {mid // 1000:>4}K tokens  actual_prompt={pt}  elapsed={elapsed:.1f}s")
            last_ok = mid
        else:
            reason = result.get("body", result.get("error", status))[:100]
            print(f"  ❌ {mid // 1000:>4}K tokens  {status}: {reason}  elapsed={elapsed:.1f}s")
            last_fail = mid

        time.sleep(2)  # 避免触发 rate limit

    print(f"\n边界: {last_ok // 1000}K ~ {last_fail // 1000}K tokens")
    return last_ok, last_fail


def linear_scan(points: list[int], history_turns: int, tool_count: int, client: httpx.Client):
    """线性扫描预设点"""
    print(f"\n=== 线性扫描: {len(points)} 个点 "
          f"(history_turns={history_turns}, tools={tool_count}) ===\n")

    results = []
    for tokens in points:
        result = test_single(tokens, history_turns, tool_count, client)
        pt = result.get("prompt_tokens", 0)
        ct = result.get("completion_tokens", 0)
        elapsed = result.get("elapsed", 0)
        status = result["status"]

        if status == "OK":
            print(f"  ✅ {tokens // 1000:>4}K  prompt={pt}  completion={ct}  {elapsed:.1f}s")
        else:
            reason = result.get("body", result.get("error", status))[:100]
            print(f"  ❌ {tokens // 1000:>4}K  {status}: {reason}  {elapsed:.1f}s")

        results.append(result)
        if status != "OK":
            break  # 第一个失败就停止
        time.sleep(1)

    return results


def main():
    parser = argparse.ArgumentParser(description="DeepSeek 网页端上下文边界探测")
    parser.add_argument("--mode", choices=["prompt", "multiturn", "tools", "linear", "binary"],
                        default="linear", help="测试模式")
    parser.add_argument("--api-base", default=API_BASE, help="API 地址")
    parser.add_argument("--start-k", type=int, default=8, help="起始 token (K)")
    parser.add_argument("--end-k", type=int, default=128, help="结束 token (K)")
    parser.add_argument("--history-turns", type=int, default=0, help="多轮历史数")
    parser.add_argument("--tool-count", type=int, default=0, help="tools 数量")
    parser.add_argument("--binary", action="store_true", help="二分查找边界")
    parser.add_argument("--single", type=int, default=0, help="仅测试单个 token 数")
    args = parser.parse_args()

    client = httpx.Client()

    # 先测试连通性
    print("检查连通性...")
    try:
        r = client.get(f"{args.api_base}/models", timeout=10)
        print(f"  服务状态: HTTP {r.status_code}")
    except Exception as e:
        print(f"  ❌ 无法连接: {e}")
        print("  请先启动服务: just serve")
        sys.exit(1)

    if args.single > 0:
        r = test_single(args.single, args.history_turns, args.tool_count, client)
        print(json.dumps(r, ensure_ascii=False, indent=2))
        return

    if args.mode == "prompt":
        args.tool_count = 0
        args.history_turns = 0
    elif args.mode == "multiturn":
        args.tool_count = 0
        args.history_turns = 16
    elif args.mode == "tools":
        args.tool_count = 10

    if args.binary or args.mode == "binary":
        binary_search(args.start_k * 1000, args.end_k * 1000,
                      args.history_turns, args.tool_count, client)
    else:
        points = [1_000, 4_000, 8_000, 12_000, 16_000, 24_000, 32_000,
                  48_000, 64_000, 80_000, 96_000, 128_000, 160_000]
        if args.start_k > 1:
            points = [p for p in points if p >= args.start_k * 1000]
        if args.end_k < 160:
            points = [p for p in points if p <= args.end_k * 1000]
        linear_scan(points, args.history_turns, args.tool_count, client)


if __name__ == "__main__":
    main()
