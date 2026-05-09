#!/usr/bin/env python3
"""工具调用评测脚本：测试 6 个场景，输出对比报告。"""

import json
import sys
import time
import urllib.request
import urllib.error

API_BASE = "http://127.0.0.1:5317"
API_KEY = "sk-e051874c4510073c9f138201485b936927ccfc4c3a667f5514598ef3d304e463"
MODEL = "deepseek-v4-flash"


def req(messages, tools, stream: bool):
    body = json.dumps({
        "model": MODEL,
        "stream": stream,
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto" if tools else None,
    }).encode()
    r = urllib.request.Request(
        f"{API_BASE}/v1/chat/completions",
        data=body,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {API_KEY}"},
        method="POST",
    )
    resp = urllib.request.urlopen(r, timeout=30)
    return resp


def parse_stream(resp):
    """解析 SSE 流，返回 (content, tool_calls)."""
    content = ""
    tool_calls = []  # list of {name, arguments, index}
    for line in resp.read().decode().split("\n"):
        if not line.startswith("data: ") or line == "data: [DONE]":
            continue
        try:
            chunk = json.loads(line[6:])
        except json.JSONDecodeError:
            continue
        for c in chunk.get("choices", []):
            delta = c.get("delta", {})
            content += delta.get("content", "") or ""
            for tc in delta.get("tool_calls", []):
                idx = tc.get("index", len(tool_calls))
                # accumulate by index
                while len(tool_calls) <= idx:
                    tool_calls.append({"name": "", "arguments": "", "index": idx})
                if tc.get("function", {}).get("name"):
                    tool_calls[idx]["name"] += tc["function"]["name"]
                if tc.get("function", {}).get("arguments"):
                    tool_calls[idx]["arguments"] += tc["function"]["arguments"]
    return content.strip(), [tc for tc in tool_calls if tc["name"]]


def parse_json(resp):
    """解析非流式响应，返回 (content, tool_calls)."""
    d = json.loads(resp.read())
    msg = d["choices"][0]["message"]
    content = (msg.get("content") or "").strip()
    tc = msg.get("tool_calls") or []
    tool_calls = [
        {"name": t["function"]["name"], "arguments": t["function"]["arguments"], "index": t.get("index", i)}
        for i, t in enumerate(tc)
    ]
    return content, tool_calls


def run_scenario(name, messages, tools, expect_tool_count=None, expect_no_content=False):
    """执行一个测试场景。"""
    start = time.time()
    err = None
    content, tool_calls = "", []

    try:
        resp = req(messages, tools, stream=True)
        content, tool_calls = parse_stream(resp)
    except Exception as e:
        err = str(e)

    elapsed = time.time() - start
    passed = True
    issues = []

    if err:
        passed = False
        issues.append(f"请求失败: {err}")
    else:
        n_tc = len(tool_calls)
        if expect_tool_count is not None:
            if n_tc != expect_tool_count:
                passed = False
                issues.append(f"期望 {expect_tool_count} 个 tool_call，实际 {n_tc}")
        if expect_no_content and content:
            passed = False
            issues.append(f"内容混入: '{content[:80]}'")
        # Check arguments are valid JSON
        for tc in tool_calls:
            try:
                json.loads(tc["arguments"])
            except json.JSONDecodeError:
                passed = False
                issues.append(f"arguments 非 JSON: '{tc['arguments'][:60]}'")
        # Check no tool tags leaked to content
        if "<|tool" in content or "tool_calls" in content.lower() and "<" in content:
            passed = False
            issues.append(f"标签泄漏到 content: '{content[-60:]}'")

    return {
        "name": name,
        "passed": passed,
        "tool_calls": len(tool_calls),
        "has_content": bool(content),
        "content_preview": content[:60],
        "elapsed": round(elapsed, 1),
        "issues": issues,
    }


def main():
    results = []
    all_passed = True

    # 场景 1: basic — 单个工具
    r = run_scenario(
        "basic",
        messages=[{"role": "user", "content": "现在几点了？帮我查时间"}],
        tools=[{
            "type": "function",
            "function": {
                "name": "get_current_time",
                "description": "获取当前时间",
                "parameters": {"type": "object", "properties": {"tz": {"type": "string"}}, "required": ["tz"]},
            },
        }],
        expect_tool_count=1,
    )
    results.append(r)

    # 场景 2: multi — 并行 2 个工具
    r = run_scenario(
        "multi",
        messages=[{"role": "user", "content": "帮我查北京的天气和上海的时间"}],
        tools=[
            {"type": "function", "function": {"name": "get_weather", "description": "获取天气", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}},
            {"type": "function", "function": {"name": "get_current_time", "description": "获取当前时间", "parameters": {"type": "object", "properties": {"tz": {"type": "string"}}, "required": ["tz"]}}},
        ],
        expect_tool_count=2,
    )
    results.append(r)

    # 场景 3: leading_text — 先说"好的"再调工具
    r = run_scenario(
        "leading_text",
        messages=[{"role": "user", "content": "帮我算 1+1 等于多少"}],
        tools=[{
            "type": "function",
            "function": {
                "name": "calculate",
                "description": "数学计算",
                "parameters": {"type": "object", "properties": {"expr": {"type": "string"}}, "required": ["expr"]},
            },
        }],
        expect_tool_count=1,
    )
    results.append(r)

    # 场景 4: nested_args — 嵌套参数
    r = run_scenario(
        "nested_args",
        messages=[{"role": "user", "content": "帮我创建一个配置文件"}],
        tools=[{
            "type": "function",
            "function": {
                "name": "create_config",
                "description": "创建配置",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "settings": {
                            "type": "object",
                            "properties": {
                                "enabled": {"type": "boolean"},
                                "items": {"type": "array", "items": {"type": "string"}},
                            },
                            "required": ["enabled"],
                        },
                    },
                    "required": ["name", "settings"],
                },
            },
        }],
        expect_tool_count=1,
    )
    results.append(r)

    # 场景 5: no_tool — 不相关问题不应触发工具
    r = run_scenario(
        "no_tool",
        messages=[{"role": "user", "content": "你好，你叫什么名字？"}],
        tools=[{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "获取天气",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
            },
        }],
        expect_tool_count=0,
    )
    results.append(r)

    # 场景 6: repeat_3x — 同一请求发 3 次
    repeat_results = []
    for i in range(3):
        r = run_scenario(
            f"repeat_{i+1}",
            messages=[{"role": "user", "content": "现在几点了？帮我查时间"}],
            tools=[{
                "type": "function",
                "function": {
                    "name": "get_current_time",
                    "description": "获取当前时间",
                    "parameters": {"type": "object", "properties": {"tz": {"type": "string"}}, "required": ["tz"]},
                },
            }],
            expect_tool_count=1,
        )
        repeat_results.append(r)

    all_repeat_pass = all(r["passed"] for r in repeat_results)
    repeat_tc = [r["tool_calls"] for r in repeat_results]
    results.append({
        "name": "repeat_3x",
        "passed": all_repeat_pass,
        "tool_calls": "/".join(str(t) for t in repeat_tc),
        "has_content": any(r["has_content"] for r in repeat_results),
        "content_preview": "",
        "elapsed": round(sum(r["elapsed"] for r in repeat_results) / 3, 1),
        "issues": [] if all_repeat_pass else [f"第 {next(i+1 for i,r in enumerate(repeat_results) if not r['passed'])} 次失败"],
    })

    # --- 输出报告 ---
    print("=== 工具调用评测报告 ===\n")
    print(f"{'场景':<14} | {'结果':<6} | {'tool_calls':<10} | {'内容':<6} | {'耗时':<6}")
    print("-" * 52)
    for r in results:
        status = "PASS" if r["passed"] else "FAIL"
        content_info = "有" if r["has_content"] else "无"
        tc_str = str(r["tool_calls"])
        print(f"{r['name']:<14} | {status:<6} | {tc_str:<10} | {content_info:<6} | {r['elapsed']}s")
        for issue in r["issues"]:
            print(f"  ⚠ {issue}")

    total_pass = sum(1 for r in results if r["passed"])
    total = len(results)
    print(f"\n总计: {total_pass}/{total} 通过")
    print(f"模型: {MODEL}")
    print(f"时间: {time.strftime('%Y-%m-%d %H:%M:%S')}")

    sys.exit(0 if all(r["passed"] for r in results) else 1)


if __name__ == "__main__":
    main()
