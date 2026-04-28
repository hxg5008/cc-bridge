//! ChatGPT 内部 Codex 端点 (`chatgpt.com/backend-api/codex/responses`) 的请求 body 改写。
//!
//! 移植自 sub2api `applyCodexOAuthTransform` 等一族函数
//! ([openai_codex_transform.go](sub2api/backend/internal/service/openai_codex_transform.go))。
//!
//! 上游对 OAuth 走的 Codex 端点有一系列硬性要求,客户端原始 body 不一定符合,例如:
//!   - `store` 必须为 false (上游不接受 `Store must be set to false`)
//!   - 不接受 `temperature` / `top_p` / `max_output_tokens` 等参数
//!   - 不接受 `role: "system"` 消息 (要提到 `instructions` 顶层字段)
//!   - 不接受 `role: "tool"` 消息 (要转换成 `function_call_output` item)
//!   - 旧版 `functions` / `function_call` 要转换为 Responses 风格的 `tools` / `tool_choice`
//!   - `instructions` 为空时要填充官方 Codex CLI 的标准提示词以匹配风控比对
//!
//! 该模块只做 JSON 改写;不发送网络请求。
use serde_json::{Map, Value};

const CODEX_INSTRUCTIONS: &str = include_str!("../../assets/codex_instructions.txt");

#[derive(Debug, Default)]
pub struct CodexTransformResult {
    /// 是否真的改写过 body (调用方据此决定是否 re-marshal)。
    pub modified: bool,
    /// 归一化后的 model 名 (例如 `gpt-5.4-high` → `gpt-5.4`),用于 prompt_cache_key 派生。
    pub normalized_model: Option<String>,
    /// 客户端在 body 中显式带过来的 `prompt_cache_key`,如果有的话原样回填。
    pub prompt_cache_key_hint: Option<String>,
}

/// OAuth 走 ChatGPT 内部 Codex 端点的 body 改写主函数。
///
/// `is_codex_cli` 为 true 表示客户端已经被识别为官方 Codex 家族(UA / Originator);
/// 当前所有 OAuth 流量都强制按 codex_cli 处理,所以一直传 true。
pub fn apply_codex_oauth_transform(
    body: &mut Value,
    _is_codex_cli: bool,
) -> CodexTransformResult {
    let mut result = CodexTransformResult::default();

    let map = match body.as_object_mut() {
        Some(m) => m,
        None => return result,
    };

    let needs_tool_continuation = needs_tool_continuation(map);

    // model trim + 归一化
    let raw_model = map.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let trimmed_model = raw_model.trim().to_string();
    if !trimmed_model.is_empty() {
        if trimmed_model != raw_model {
            map.insert("model".into(), Value::String(trimmed_model.clone()));
            result.modified = true;
        }
        result.normalized_model = Some(trimmed_model);
    }

    // store=false / stream=true 强制
    if map.get("store").and_then(|v| v.as_bool()).unwrap_or(true) {
        map.insert("store".into(), Value::Bool(false));
        result.modified = true;
    }
    if !map.get("stream").and_then(|v| v.as_bool()).unwrap_or(false) {
        map.insert("stream".into(), Value::Bool(true));
        result.modified = true;
    }

    // 删除 Codex 上游不支持的参数
    for key in [
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "prompt_cache_retention",
    ] {
        if map.remove(key).is_some() {
            result.modified = true;
        }
    }

    // legacy functions[] → tools[{type:"function", function:f}]
    if let Some(Value::Array(functions)) = map.remove("functions") {
        let tools: Vec<Value> = functions
            .into_iter()
            .map(|f| {
                let mut t = Map::new();
                t.insert("type".into(), Value::String("function".into()));
                t.insert("function".into(), f);
                Value::Object(t)
            })
            .collect();
        map.insert("tools".into(), Value::Array(tools));
        result.modified = true;
    }

    // legacy function_call → tool_choice
    if let Some(fc) = map.remove("function_call") {
        match fc {
            Value::String(s) => {
                map.insert("tool_choice".into(), Value::String(s));
            }
            Value::Object(obj) => {
                if let Some(name) =
                    obj.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
                {
                    let mut tc = Map::new();
                    tc.insert("type".into(), Value::String("function".into()));
                    let mut func = Map::new();
                    func.insert("name".into(), Value::String(name.to_string()));
                    tc.insert("function".into(), Value::Object(func));
                    map.insert("tool_choice".into(), Value::Object(tc));
                }
            }
            _ => {}
        }
        result.modified = true;
    }

    if normalize_codex_tools(map) {
        result.modified = true;
    }
    if normalize_codex_tool_choice(map) {
        result.modified = true;
    }

    // 客户端原始 prompt_cache_key (如果有)
    if let Some(s) = map.get("prompt_cache_key").and_then(|v| v.as_str()) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            result.prompt_cache_key_hint = Some(trimmed.to_string());
        }
    }

    // input 里 role=system 的消息提取到 instructions
    if extract_system_messages_from_input(map) {
        result.modified = true;
    }

    // instructions 为空就填官方 Codex CLI 提示词
    if apply_default_instructions(map) {
        result.modified = true;
    }

    // input 改写: tool role → function_call_output / 文本兜底 / item 过滤
    if let Some(input_value) = map.get("input").cloned() {
        match input_value {
            Value::Array(input_arr) => {
                let (mut input_arr, role_modified) = normalize_codex_tool_role_messages(input_arr);
                if role_modified {
                    result.modified = true;
                }
                let (input_after_text, text_modified) = normalize_codex_message_content_text(input_arr);
                input_arr = input_after_text;
                if text_modified {
                    result.modified = true;
                }
                let filtered = filter_codex_input(input_arr, needs_tool_continuation);
                map.insert("input".into(), Value::Array(filtered));
                result.modified = true;
            }
            Value::String(s) => {
                let trimmed = s.trim();
                let new_input = if trimmed.is_empty() {
                    Value::Array(vec![])
                } else {
                    let mut item = Map::new();
                    item.insert("type".into(), Value::String("message".into()));
                    item.insert("role".into(), Value::String("user".into()));
                    item.insert("content".into(), Value::String(s.clone()));
                    Value::Array(vec![Value::Object(item)])
                };
                map.insert("input".into(), new_input);
                result.modified = true;
            }
            _ => {}
        }
    }

    result
}

// ---------------------------------------------------------------------------
// 模型归一化
// ---------------------------------------------------------------------------

/// `gpt-5.4-high` / `gpt-5.4-medium` 等带推理强度后缀的别名 → 基础 codex 模型名。
///
/// 移植自 [normalizeCodexModel](sub2api/backend/internal/service/openai_codex_transform.go#L389)。
pub fn normalize_codex_model(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return "gpt-5.4".to_string();
    }

    // 取 `/` 后最后一段 (兼容 `provider/model` 写法)
    let model_id = model.rsplit('/').next().unwrap_or(model).trim();
    if let Some(mapped) = lookup_codex_model_map(model_id) {
        return mapped.to_string();
    }

    let normalized = model_id.to_lowercase();
    if normalized.contains("gpt-5.5") || normalized.contains("gpt 5.5") {
        return "gpt-5.5".to_string();
    }
    if normalized.contains("gpt-5.4-mini") || normalized.contains("gpt 5.4 mini") {
        return "gpt-5.4-mini".to_string();
    }
    if normalized.contains("gpt-5.4") || normalized.contains("gpt 5.4") {
        return "gpt-5.4".to_string();
    }
    if normalized.contains("gpt-5.2") || normalized.contains("gpt 5.2") {
        return "gpt-5.2".to_string();
    }
    if normalized.contains("gpt-5.3-codex-spark") || normalized.contains("gpt 5.3 codex spark") {
        return "gpt-5.3-codex-spark".to_string();
    }
    if normalized.contains("gpt-5.3-codex") || normalized.contains("gpt 5.3 codex") {
        return "gpt-5.3-codex".to_string();
    }
    if normalized.contains("gpt-5.3") || normalized.contains("gpt 5.3") {
        return "gpt-5.3-codex".to_string();
    }
    if normalized.contains("codex") {
        return "gpt-5.3-codex".to_string();
    }
    if normalized.contains("gpt-5") || normalized.contains("gpt 5") {
        return "gpt-5.4".to_string();
    }

    "gpt-5.4".to_string()
}

fn lookup_codex_model_map(model_id: &str) -> Option<&'static str> {
    // 移植自 codexModelMap (openai_codex_transform.go:9-41)
    let table: &[(&str, &str)] = &[
        ("gpt-5.5", "gpt-5.5"),
        ("gpt-5.4", "gpt-5.4"),
        ("gpt-5.4-mini", "gpt-5.4-mini"),
        ("gpt-5.4-none", "gpt-5.4"),
        ("gpt-5.4-low", "gpt-5.4"),
        ("gpt-5.4-medium", "gpt-5.4"),
        ("gpt-5.4-high", "gpt-5.4"),
        ("gpt-5.4-xhigh", "gpt-5.4"),
        ("gpt-5.4-chat-latest", "gpt-5.4"),
        ("gpt-5.3", "gpt-5.3-codex"),
        ("gpt-5.3-none", "gpt-5.3-codex"),
        ("gpt-5.3-low", "gpt-5.3-codex"),
        ("gpt-5.3-medium", "gpt-5.3-codex"),
        ("gpt-5.3-high", "gpt-5.3-codex"),
        ("gpt-5.3-xhigh", "gpt-5.3-codex"),
        ("gpt-5.3-codex", "gpt-5.3-codex"),
        ("gpt-5.3-codex-spark", "gpt-5.3-codex-spark"),
        ("gpt-5.3-codex-spark-low", "gpt-5.3-codex-spark"),
        ("gpt-5.3-codex-spark-medium", "gpt-5.3-codex-spark"),
        ("gpt-5.3-codex-spark-high", "gpt-5.3-codex-spark"),
        ("gpt-5.3-codex-spark-xhigh", "gpt-5.3-codex-spark"),
        ("gpt-5.3-codex-low", "gpt-5.3-codex"),
        ("gpt-5.3-codex-medium", "gpt-5.3-codex"),
        ("gpt-5.3-codex-high", "gpt-5.3-codex"),
        ("gpt-5.3-codex-xhigh", "gpt-5.3-codex"),
        ("gpt-5.2", "gpt-5.2"),
        ("gpt-5.2-none", "gpt-5.2"),
        ("gpt-5.2-low", "gpt-5.2"),
        ("gpt-5.2-medium", "gpt-5.2"),
        ("gpt-5.2-high", "gpt-5.2"),
        ("gpt-5.2-xhigh", "gpt-5.2"),
    ];
    for (k, v) in table {
        if *k == model_id {
            return Some(v);
        }
    }
    let lower = model_id.to_lowercase();
    for (k, v) in table {
        if k.eq_ignore_ascii_case(&lower) {
            return Some(v);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 子步骤实现
// ---------------------------------------------------------------------------

fn extract_system_messages_from_input(map: &mut Map<String, Value>) -> bool {
    let input = match map.get("input") {
        Some(Value::Array(a)) => a.clone(),
        _ => return false,
    };
    if input.is_empty() {
        return false;
    }

    let mut system_texts: Vec<String> = Vec::new();
    let mut remaining: Vec<Value> = Vec::with_capacity(input.len());

    for item in input {
        let role = item
            .as_object()
            .and_then(|m| m.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if role != "system" {
            remaining.push(item);
            continue;
        }
        let content = item.as_object().and_then(|m| m.get("content")).cloned().unwrap_or(Value::Null);
        let text = extract_text_from_content(&content);
        if !text.is_empty() {
            system_texts.push(text);
        }
    }

    if system_texts.is_empty() {
        return false;
    }

    let extracted = system_texts.join("\n\n");
    let merged = match map.get("instructions") {
        Some(Value::String(s)) if !s.trim().is_empty() => format!("{}\n\n{}", extracted, s),
        _ => extracted,
    };
    map.insert("instructions".into(), Value::String(merged));
    map.insert("input".into(), Value::Array(remaining));
    true
}

fn apply_default_instructions(map: &mut Map<String, Value>) -> bool {
    let is_empty = match map.get("instructions") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        _ => true,
    };
    if !is_empty {
        return false;
    }
    map.insert(
        "instructions".into(),
        Value::String(CODEX_INSTRUCTIONS.to_string()),
    );
    true
}

fn normalize_codex_tools(map: &mut Map<String, Value>) -> bool {
    let tools = match map.get_mut("tools") {
        Some(Value::Array(a)) => a,
        _ => return false,
    };
    let mut modified = false;
    let mut valid: Vec<Value> = Vec::with_capacity(tools.len());

    for tool in tools.drain(..) {
        let mut tool_map = match tool {
            Value::Object(m) => m,
            other => {
                valid.push(other);
                continue;
            }
        };
        let tool_type = tool_map.get("type").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if tool_type != "function" {
            valid.push(Value::Object(tool_map));
            continue;
        }
        // Responses 风格: 顶层有非空 name
        let has_top_name = tool_map
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if has_top_name {
            valid.push(Value::Object(tool_map));
            continue;
        }
        // ChatCompletions 风格: function: { name, ... }
        let function = match tool_map.get("function").cloned() {
            Some(Value::Object(f)) => f,
            _ => {
                modified = true;
                continue; // drop invalid function tool
            }
        };
        if !tool_map.contains_key("name") {
            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                if !name.trim().is_empty() {
                    tool_map.insert("name".into(), Value::String(name.to_string()));
                    modified = true;
                }
            }
        }
        if !tool_map.contains_key("description") {
            if let Some(desc) = function.get("description").and_then(|v| v.as_str()) {
                if !desc.trim().is_empty() {
                    tool_map.insert("description".into(), Value::String(desc.to_string()));
                    modified = true;
                }
            }
        }
        if !tool_map.contains_key("parameters") {
            if let Some(params) = function.get("parameters").cloned() {
                tool_map.insert("parameters".into(), params);
                modified = true;
            }
        }
        if !tool_map.contains_key("strict") {
            if let Some(strict) = function.get("strict").cloned() {
                tool_map.insert("strict".into(), strict);
                modified = true;
            }
        }
        valid.push(Value::Object(tool_map));
    }
    if modified {
        map.insert("tools".into(), Value::Array(valid));
    }
    modified
}

fn normalize_codex_tool_choice(map: &mut Map<String, Value>) -> bool {
    let choice = match map.get("tool_choice") {
        Some(Value::Object(c)) => c.clone(),
        _ => return false,
    };
    let choice_type = choice.get("type").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if choice_type.is_empty() {
        return false;
    }
    if codex_tools_contain_type(map.get("tools"), &choice_type) {
        return false;
    }
    map.insert("tool_choice".into(), Value::String("auto".into()));
    true
}

fn codex_tools_contain_type(tools: Option<&Value>, target_type: &str) -> bool {
    let arr = match tools {
        Some(Value::Array(a)) => a,
        _ => return false,
    };
    for t in arr {
        if let Some(tt) = t.get("type").and_then(|v| v.as_str()) {
            if tt.trim() == target_type {
                return true;
            }
        }
    }
    false
}

fn normalize_codex_tool_role_messages(input: Vec<Value>) -> (Vec<Value>, bool) {
    if input.is_empty() {
        return (input, false);
    }
    let mut modified = false;
    let mut out: Vec<Value> = Vec::with_capacity(input.len());

    for item in input {
        let m = match item.as_object() {
            Some(o) => o.clone(),
            None => {
                out.push(item);
                continue;
            }
        };
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if role != "tool" {
            out.push(item);
            continue;
        }
        let call_id = first_non_empty_str(&[
            m.get("call_id"),
            m.get("tool_call_id"),
            m.get("id"),
        ])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

        if call_id.is_empty() {
            // 没 call_id 兜底为 user 消息
            let mut fallback = m.clone();
            fallback.insert("role".into(), Value::String("user".into()));
            fallback.remove("tool_call_id");
            out.push(Value::Object(fallback));
            modified = true;
            continue;
        }

        let mut output_text = extract_text_from_content(m.get("content").unwrap_or(&Value::Null));
        if output_text.is_empty() {
            if let Some(Value::String(s)) = m.get("output") {
                output_text = s.clone();
            }
        }
        if output_text.is_empty() {
            if let Some(content) = m.get("content") {
                if !matches!(content, Value::Null) {
                    output_text = serde_json::to_string(content).unwrap_or_default();
                }
            }
        }

        let mut new_item = Map::new();
        new_item.insert("type".into(), Value::String("function_call_output".into()));
        new_item.insert("call_id".into(), Value::String(call_id));
        new_item.insert("output".into(), Value::String(output_text));
        out.push(Value::Object(new_item));
        modified = true;
    }
    if !modified {
        return (out, false);
    }
    (out, true)
}

fn normalize_codex_message_content_text(input: Vec<Value>) -> (Vec<Value>, bool) {
    if input.is_empty() {
        return (input, false);
    }
    let mut modified = false;
    let mut out: Vec<Value> = Vec::with_capacity(input.len());

    for item in input {
        let m = match item.as_object() {
            Some(o) => o.clone(),
            None => {
                out.push(item);
                continue;
            }
        };
        let item_type = m.get("type").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if item_type != "message" {
            out.push(item);
            continue;
        }
        let parts = match m.get("content") {
            Some(Value::Array(arr)) => arr.clone(),
            _ => {
                out.push(item);
                continue;
            }
        };

        let mut new_parts = parts.clone();
        let mut item_changed = false;
        for (i, part) in parts.iter().enumerate() {
            let part_map = match part.as_object() {
                Some(p) => p,
                None => continue,
            };
            let text_val = match part_map.get("text") {
                Some(v) => v.clone(),
                None => continue,
            };
            if matches!(text_val, Value::String(_)) {
                continue;
            }
            let stringified = stringify_codex_content_text(&text_val);
            let mut new_part = part_map.clone();
            new_part.insert("text".into(), Value::String(stringified));
            new_parts[i] = Value::Object(new_part);
            item_changed = true;
        }

        if item_changed {
            let mut new_item = m;
            new_item.insert("content".into(), Value::Array(new_parts));
            out.push(Value::Object(new_item));
            modified = true;
        } else {
            out.push(item);
        }
    }
    if !modified {
        return (out, false);
    }
    (out, true)
}

fn stringify_codex_content_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_else(|_| format!("{}", other)),
    }
}

fn filter_codex_input(input: Vec<Value>, preserve_references: bool) -> Vec<Value> {
    let mut out = Vec::with_capacity(input.len());
    for item in input {
        let m = match item.as_object() {
            Some(o) => o.clone(),
            None => {
                out.push(item);
                continue;
            }
        };
        let item_type = m.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();

        if item_type == "item_reference" {
            if !preserve_references {
                continue;
            }
            let mut new_item = m.clone();
            if let Some(id) = new_item.get("id").and_then(|v| v.as_str()) {
                if id.starts_with("call_") {
                    new_item.insert("id".into(), Value::String(fix_call_id_prefix(id)));
                }
            }
            out.push(Value::Object(new_item));
            continue;
        }

        let mut new_item = m.clone();
        let mut copied = false;
        let ensure_copy = |target: &mut Map<String, Value>, original: &Map<String, Value>, copied: &mut bool| {
            if *copied {
                return;
            }
            *target = original.clone();
            *copied = true;
        };

        if is_codex_tool_call_item_type(&item_type) {
            let mut call_id = m.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if call_id.trim().is_empty() {
                if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                    if !id.trim().is_empty() {
                        call_id = id.to_string();
                        ensure_copy(&mut new_item, &m, &mut copied);
                        new_item.insert("call_id".into(), Value::String(call_id.clone()));
                    }
                }
            }
            if !call_id.is_empty() {
                let fixed = fix_call_id_prefix(&call_id);
                if fixed != call_id {
                    ensure_copy(&mut new_item, &m, &mut copied);
                    new_item.insert("call_id".into(), Value::String(fixed));
                }
            }
        }

        if !is_codex_tool_call_item_type(&item_type) {
            ensure_copy(&mut new_item, &m, &mut copied);
            new_item.remove("call_id");
        }

        if codex_input_item_requires_name(&item_type) {
            let has_name = new_item
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !has_name {
                let mut name = first_non_empty_str(&[m.get("tool_name")])
                    .map(str::to_string)
                    .unwrap_or_default();
                if name.is_empty() {
                    if let Some(func) = m.get("function").and_then(|v| v.as_object()) {
                        if let Some(n) = func.get("name").and_then(|v| v.as_str()) {
                            if !n.trim().is_empty() {
                                name = n.to_string();
                            }
                        }
                    }
                }
                if name.is_empty() {
                    name = "tool".to_string();
                }
                ensure_copy(&mut new_item, &m, &mut copied);
                new_item.insert("name".into(), Value::String(name));
            }
        }

        if !preserve_references {
            ensure_copy(&mut new_item, &m, &mut copied);
            new_item.remove("id");
        }

        out.push(Value::Object(new_item));
    }
    out
}

fn fix_call_id_prefix(id: &str) -> String {
    if id.is_empty() || id.starts_with("fc") {
        return id.to_string();
    }
    if let Some(stripped) = id.strip_prefix("call_") {
        return format!("fc{}", stripped);
    }
    format!("fc_{}", id)
}

fn is_codex_tool_call_item_type(typ: &str) -> bool {
    matches!(
        typ,
        "function_call"
            | "tool_call"
            | "local_shell_call"
            | "tool_search_call"
            | "custom_tool_call"
            | "mcp_tool_call"
            | "function_call_output"
            | "mcp_tool_call_output"
            | "custom_tool_call_output"
            | "tool_search_output"
    )
}

fn codex_input_item_requires_name(typ: &str) -> bool {
    matches!(typ.trim(), "function_call" | "custom_tool_call" | "mcp_tool_call")
}

// ---------------------------------------------------------------------------
// 续链信号 (NeedsToolContinuation)
// ---------------------------------------------------------------------------

fn needs_tool_continuation(map: &Map<String, Value>) -> bool {
    if has_non_empty_string(map.get("previous_response_id")) {
        return true;
    }
    if has_tools_signal(map) {
        return true;
    }
    if has_tool_choice_signal(map) {
        return true;
    }
    let input = match map.get("input") {
        Some(Value::Array(a)) => a,
        _ => return false,
    };
    for item in input {
        let typ = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if is_codex_tool_call_item_type(typ) || typ == "item_reference" {
            return true;
        }
    }
    false
}

fn has_tools_signal(map: &Map<String, Value>) -> bool {
    matches!(map.get("tools"), Some(Value::Array(a)) if !a.is_empty())
}

fn has_tool_choice_signal(map: &Map<String, Value>) -> bool {
    match map.get("tool_choice") {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => false,
    }
}

fn has_non_empty_string(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if !s.trim().is_empty())
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn extract_text_from_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(arr) => {
            let mut parts: Vec<String> = Vec::new();
            for item in arr {
                let m = match item.as_object() {
                    Some(o) => o,
                    None => continue,
                };
                let typ = m.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if typ == "text" || typ == "input_text" || typ == "output_text" {
                    if let Some(t) = m.get("text").and_then(|v| v.as_str()) {
                        parts.push(t.to_string());
                    }
                }
            }
            parts.join("")
        }
        _ => String::new(),
    }
}

fn first_non_empty_str<'a>(values: &[Option<&'a Value>]) -> Option<&'a str> {
    for v in values {
        if let Some(Value::String(s)) = v {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(s.as_str());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forces_store_false_and_stream_true() {
        let mut body = json!({
            "model": "gpt-5.4",
            "store": true,
            "stream": false,
            "input": []
        });
        let r = apply_codex_oauth_transform(&mut body, true);
        assert!(r.modified);
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
    }

    #[test]
    fn strips_unsupported_parameters() {
        let mut body = json!({
            "model": "gpt-5.4",
            "temperature": 0.5,
            "top_p": 0.9,
            "max_output_tokens": 100,
            "max_completion_tokens": 100,
            "frequency_penalty": 0.1,
            "presence_penalty": 0.1,
            "prompt_cache_retention": "session",
            "input": []
        });
        apply_codex_oauth_transform(&mut body, true);
        for k in [
            "temperature",
            "top_p",
            "max_output_tokens",
            "max_completion_tokens",
            "frequency_penalty",
            "presence_penalty",
            "prompt_cache_retention",
        ] {
            assert!(body.get(k).is_none(), "{} should be removed", k);
        }
    }

    #[test]
    fn extracts_system_role_to_instructions() {
        let mut body = json!({
            "model": "gpt-5.4",
            "input": [
                {"role": "system", "content": "you are foo"},
                {"role": "user", "content": "hello"}
            ]
        });
        apply_codex_oauth_transform(&mut body, true);
        let instr = body["instructions"].as_str().unwrap();
        assert!(instr.starts_with("you are foo"));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn fills_default_instructions_when_empty() {
        let mut body = json!({"model": "gpt-5.4", "input": []});
        apply_codex_oauth_transform(&mut body, true);
        let instr = body["instructions"].as_str().unwrap();
        assert!(instr.contains("Codex"), "default instructions must contain 'Codex'");
    }

    #[test]
    fn converts_legacy_functions_to_tools() {
        let mut body = json!({
            "model": "gpt-5.4",
            "functions": [{"name": "f1"}],
            "function_call": "auto",
            "input": []
        });
        apply_codex_oauth_transform(&mut body, true);
        assert!(body.get("functions").is_none());
        assert!(body.get("function_call").is_none());
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn converts_tool_role_to_function_call_output() {
        let mut body = json!({
            "model": "gpt-5.4",
            "tools": [{"type": "function", "name": "f"}],
            "input": [
                {"role": "tool", "tool_call_id": "call_abc", "content": "result"}
            ]
        });
        apply_codex_oauth_transform(&mut body, true);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"].as_str().unwrap(), "fcabc");
        assert_eq!(input[0]["output"].as_str().unwrap(), "result");
    }

    #[test]
    fn string_input_becomes_message_array() {
        let mut body = json!({"model": "gpt-5.4", "input": "hello"});
        apply_codex_oauth_transform(&mut body, true);
        let arr = body["input"].as_array().unwrap();
        assert_eq!(arr[0]["type"], "message");
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "hello");
    }

    #[test]
    fn normalize_model_handles_variants() {
        assert_eq!(normalize_codex_model("gpt-5.4-high"), "gpt-5.4");
        assert_eq!(normalize_codex_model("gpt-5.3"), "gpt-5.3-codex");
        assert_eq!(normalize_codex_model("gpt-5.4-mini"), "gpt-5.4-mini");
        assert_eq!(normalize_codex_model("gpt-5.5"), "gpt-5.5");
        assert_eq!(normalize_codex_model("openai/gpt-5.4-high"), "gpt-5.4");
        assert_eq!(normalize_codex_model("codex"), "gpt-5.3-codex");
        assert_eq!(normalize_codex_model(""), "gpt-5.4");
    }

    #[test]
    fn keeps_prompt_cache_key_hint() {
        let mut body = json!({
            "model": "gpt-5.4",
            "prompt_cache_key": "my-key",
            "input": []
        });
        let r = apply_codex_oauth_transform(&mut body, true);
        assert_eq!(r.prompt_cache_key_hint.as_deref(), Some("my-key"));
        // 不删
        assert_eq!(body["prompt_cache_key"], "my-key");
    }
}
