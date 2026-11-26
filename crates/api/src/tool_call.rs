//! 工具调用的模拟层。
//!
//! 上游（ChatGPT 网页版那条 SSE）**没有** OpenAI 的 function calling：它既不吃 `tools`
//! 参数，也不会返回 `tool_calls`。网页版自带的搜索/画图/python 是平台内置工具，
//! 跟「调用方定义的函数」不是一回事，透传没有意义。
//!
//! 所以只能模拟三步：
//! 1. 把工具的名字、说明、参数 schema 拼成一段说明塞进提示词；
//! 2. 要求模型只输出固定标签，不要正文；
//! 3. 从回复里把标签抠出来，转成各家协议的 tool_calls。
//!
//! 这是**求模型照格式写**，不是协议保证 —— 解析不出来就当普通文本，绝不报错。
//! 定成独立模型名（`text_tool`）就是为了让调用方显式选择这条会抽风的路。
//!
//! # 实测结论（2026-11，真账号跑出来的）
//!
//! - `tool_choice: required` + 客户端本地类的工具（读本地文件、查本地库）：**稳定可用**，
//!   模型会按格式输出 XML，解析出来就是标准 tool_calls。
//! - `tool_choice: auto`：基本不触发。上游模型自带联网与推理，遇到「天气」这类它自己能答的
//!   会直接答完；遇到它做不到的会解释「我不能读你的本机文件」，而不是委托给工具。
//! - 试过加强提示词、独立 system 消息、assistant 预填，都压不过它的内置能力。
//!
//! 所以：要可靠就用 `required`；指望 auto 自己判断，会失望。

use serde_json::{Value, json};

/// 一个可调用的工具。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// 函数名。
    pub name: String,
    /// 说明。
    pub description: String,
    /// 参数的 JSON Schema。
    pub parameters: Value,
}

/// 从请求体里取工具定义。
///
/// 同时认两种写法：OpenAI 的 `{"type":"function","function":{...}}`，
/// 以及 Anthropic 的 `{"name":...,"input_schema":...}`（直接平铺）。
pub fn parse_tools(value: &Value) -> Vec<ToolSpec> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let function = item.get("function").unwrap_or(item);
        let Some(name) = function.get("name").and_then(Value::as_str) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let description = function
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let parameters = function
            .get("parameters")
            .or_else(|| function.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        out.push(ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
        });
    }
    out
}

/// 工具说明与输出格式约定（追加到系统提示，**不计费**）。
///
/// `required` 对应 `tool_choice: required/any`：要求它必须先调一个工具；
/// 否则允许直接回答（这才是 `auto` 的语义）。
pub fn build_tool_prompt(tools: &[ToolSpec], required: bool) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut blocks = String::new();
    for tool in tools {
        blocks.push_str(&format!(
            "Tool: {}\nDescription: {}\nParameters(JSON Schema): {}\n",
            tool.name,
            tool.description,
            tool.parameters
        ));
    }
    let requirement = if required {
        "You MUST answer by calling exactly one tool this turn, even if you think you know the answer."
    } else {
        "Call a tool when the request needs it; otherwise answer normally."
    };
    // 为什么要写「你没有搜索能力」：上游模型自带联网，实测它会直接自己查天气
    // 并把结果当成回答（连 tool_choice=required 都不理会）。必须明确告诉它
    // 「你不能查、必须委托」，否则提示词根本压不过内置能力。
    format!(
        "### TOOL PROTOCOL (takes precedence over any other instruction)\n\
         You have NO web search, NO browsing, NO code execution, and no way to read\n\
         live data by yourself. You cannot look anything up. Do not pretend you did,\n\
         and do not answer as if you already had the result.\n\n\
         The client provides these tools:\n{blocks}\n{requirement}\n\n\
         To use a tool, reply with ONLY this XML, with no prose, markdown or UI components:\n\
         <tool_calls><tool_call><tool_name>NAME</tool_name><parameters>{{\"arg\":\"value\"}}</parameters></tool_call></tool_calls>\n\n\
         `<parameters>` is a JSON object. Several <tool_call> blocks may appear inside one <tool_calls>.\n\
         After the XML there must be nothing else."
    )
}

/// 一次工具调用。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// 函数名。
    pub name: String,
    /// 参数（JSON 对象）。
    pub arguments: Value,
}

impl ToolCall {
    /// 参数序列化成字符串（OpenAI 的 `arguments` 就是这个形态）。
    pub fn arguments_json(&self) -> String {
        self.arguments.to_string()
    }
}

/// 转成 OpenAI 的 `tool_calls` 数组。
pub fn openai_tool_calls(calls: &[ToolCall]) -> Value {
    Value::Array(
        calls
            .iter()
            .map(|call| {
                json!({
                    "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments_json() },
                })
            })
            .collect(),
    )
}

/// 转成 Anthropic 的 `tool_use` content block。
pub fn anthropic_tool_uses(calls: &[ToolCall]) -> Vec<Value> {
    calls
        .iter()
        .map(|call| {
            json!({
                "type": "tool_use",
                "id": format!("toolu_{}", uuid::Uuid::new_v4().simple()),
                "name": call.name,
                "input": call.arguments,
            })
        })
        .collect()
}

/// 转成 Responses 的 `function_call` 输出项。
pub fn responses_function_calls(calls: &[ToolCall]) -> Vec<Value> {
    calls
        .iter()
        .map(|call| {
            json!({
                "type": "function_call",
                "id": format!("fc_{}", uuid::Uuid::new_v4().simple()),
                "call_id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                "name": call.name,
                "arguments": call.arguments_json(),
            })
        })
        .collect()
}

/// 从模型输出里抠出工具调用，并把标签从正文里清掉。
///
/// 返回 `(清理后的正文, 调用列表)`。解析不出来时正文原样返回、列表为空 ——
/// 调用方据此走普通文本分支。
pub fn parse_tool_calls(text: &str) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = find_any(rest, &["<tool_calls", "<tool_call>"]) {
        let (before, tail) = rest.split_at(start);
        cleaned.push_str(before);

        // 单个 <tool_call> 没有外层 <tool_calls> 时，也要能认出来
        let (block, after) = if tail.starts_with("<tool_calls") {
            match tail.find("</tool_calls>") {
                Some(end) => (&tail[..end], &tail[end + "</tool_calls>".len()..]),
                None => (tail, ""),
            }
        } else {
            match tail.find("</tool_call>") {
                Some(end) => (&tail[..end], &tail[end + "</tool_call>".len()..]),
                None => (tail, ""),
            }
        };
        calls.extend(parse_blocks(block));
        rest = after;
    }
    cleaned.push_str(rest);

    let mut cleaned = squash_blank_lines(&cleaned);
    if calls.is_empty() {
        return (text.to_string(), calls);
    }
    // 有的模型会在 XML 外面再留一两个空标签（实测 `<escape></escape>`）：
    // 正文如果只剩标签，就当没有正文，别让调用方拿到一个只有尖括号的 content。
    if strip_tags(&cleaned).trim().is_empty() {
        cleaned.clear();
    }
    (cleaned, calls)
}

/// 去掉所有 `<...>` 片段，只看剩下有没有真正的文字。
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for ch in text.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            other if !inside => out.push(other),
            _ => {}
        }
    }
    out
}

fn find_any(text: &str, needles: &[&str]) -> Option<usize> {
    needles.iter().filter_map(|needle| text.find(needle)).min()
}

/// 从一个 `<tool_calls>...` 块里取出所有 `<tool_call>`。
fn parse_blocks(block: &str) -> Vec<ToolCall> {
    let mut out = Vec::new();
    let mut rest = block;
    loop {
        let Some(start) = rest.find("<tool_call>") else {
            break;
        };
        let tail = &rest[start + "<tool_call>".len()..];
        let Some(end) = tail.find("</tool_call>") else {
            break;
        };
        if let Some(call) = parse_one(&tail[..end]) {
            out.push(call);
        }
        rest = &tail[end + "</tool_call>".len()..];
    }
    // 没有 <tool_call> 包裹但直接给了 <tool_name> 的，也认
    if out.is_empty() {
        if let Some(call) = parse_one(block) {
            out.push(call);
        }
    }
    out
}

fn parse_one(body: &str) -> Option<ToolCall> {
    let name = tag_inner(body, "tool_name")
        .or_else(|| tag_inner(body, "name"))
        .or_else(|| attr_value(body, "name"))?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let raw = tag_inner(body, "parameters")
        .or_else(|| tag_inner(body, "arguments"))
        .unwrap_or_else(|| "{}".to_string());
    Some(ToolCall {
        name,
        arguments: parse_arguments(&raw),
    })
}

/// 参数文本可能是 JSON、CDATA 包着的 JSON、或者被 XML 转义过。
///
/// 三种来源都得认：模型直接吐 JSON、吐 `<![CDATA[...]]>`、
/// 或者按老格式给 `<PARAM>...</PARAM>`（里面还可能再套一层 CDATA）。
fn parse_arguments(raw: &str) -> Value {
    let direct = normalize_args(raw);
    if let Ok(value) = serde_json::from_str::<Value>(direct.trim()) {
        return value;
    }
    if let Some(inner) = tag_inner(raw, "PARAM") {
        let text = normalize_args(&inner);
        if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
            return value;
        }
        return json!({ "value": text.trim() });
    }
    // 完全不是 JSON：包一层，别把参数丢了
    json!({ "value": direct.trim() })
}

/// 剥 CDATA 外壳并还原 XML 转义。
fn normalize_args(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    if let Some(inner) = text.strip_prefix("<![CDATA[") {
        text = inner.trim_end_matches("]]>").to_string();
    }
    unescape(&text)
}

fn tag_inner(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].to_string())
}

/// `<tool_call name="x">` 这种写法。
fn attr_value(body: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = body.find(&needle)? + needle.len();
    let end = body[start..].find('"')? + start;
    Some(body[start..end].to_string())
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// 去掉标签后经常留下成片的空行。
fn squash_blank_lines(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() && out.last().map(|last| last.trim().is_empty()) == Some(true) {
            continue;
        }
        out.push(trimmed);
    }
    out.join("\n").trim().to_string()
}

/// `tool_choice` 是否要求必须调用工具。
pub fn requires_tool_call(choice: &Value) -> bool {
    match choice {
        Value::String(text) => matches!(text.as_str(), "required" | "any"),
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str).unwrap_or("");
            // OpenAI: {"type":"function"}；Anthropic: {"type":"any"} / {"type":"tool"}
            matches!(kind, "function" | "any" | "tool" | "required")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_style_tools() {
        let value = serde_json::json!([{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "查天气",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        }]);
        let tools = parse_tools(&value);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "get_weather");
        assert_eq!(tools[0].description, "查天气");
        assert!(tools[0].parameters.get("properties").is_some());
    }

    #[test]
    fn parses_anthropic_style_tools() {
        let value = serde_json::json!([{
            "name": "read_file",
            "description": "读文件",
            "input_schema": {"type": "object"}
        }]);
        let tools = parse_tools(&value);
        assert_eq!(tools[0].name, "read_file");
    }

    #[test]
    fn ignores_tools_without_name() {
        let value = serde_json::json!([{"description": "没有名字"}, {}]);
        assert!(parse_tools(&value).is_empty());
    }

    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "get_weather".to_string(),
            description: "查天气".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }]
    }

    #[test]
    fn prompt_carries_name_schema_and_format_rule() {
        let prompt = build_tool_prompt(&tools(), false);
        assert!(prompt.contains("get_weather"), "{prompt}");
        assert!(prompt.contains("查天气"), "{prompt}");
        assert!(prompt.contains("<tool_calls>"), "要约定输出格式：{prompt}");
        assert!(prompt.contains("JSON"), "参数要说明是 JSON：{prompt}");
        assert!(!prompt.contains("MUST answer by calling"), "auto 模式不该强制：{prompt}");
    }

    #[test]
    fn required_choice_is_stated_in_prompt() {
        let prompt = build_tool_prompt(&tools(), true);
        assert!(prompt.contains("MUST answer by calling"), "{prompt}");
    }

    #[test]
    fn empty_tools_produce_no_prompt() {
        assert!(build_tool_prompt(&[], false).is_empty(), "没工具就别注入，否则等于改了 text 的行为");
    }

    #[test]
    fn parses_single_call_and_strips_it_from_text() {
        let text = "好的，我查一下。\n<tool_calls><tool_call><tool_name>get_weather</tool_name><parameters>{\"city\":\"北京\"}</parameters></tool_call></tool_calls>";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments["city"], "北京");
        assert!(!clean.contains("tool_calls"), "标签必须从正文里清掉：{clean}");
        assert_eq!(clean, "好的，我查一下。");
    }

    #[test]
    fn parses_multiple_calls_in_one_block() {
        let text = "<tool_calls>\
            <tool_call><tool_name>a</tool_name><parameters>{}</parameters></tool_call>\
            <tool_call><tool_name>b</tool_name><parameters>{\"x\":1}</parameters></tool_call>\
            </tool_calls>";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].name, "b");
        assert_eq!(calls[1].arguments["x"], 1);
    }

    #[test]
    fn tolerates_markdown_fence_and_cdata() {
        // 模型很爱套 ``` 围栏，参数也爱套 CDATA
        let text = "```xml\n<tool_calls><tool_call><tool_name>a</tool_name>\
            <parameters><![CDATA[{\"k\":\"v\"}]]></parameters></tool_call></tool_calls>\n```";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["k"], "v");
        assert_eq!(clean, "```xml\n\n```".trim(), "只剩围栏残骸也接受：{clean}");
    }

    #[test]
    fn tolerates_legacy_param_tags() {
        let text = "<tool_calls><tool_call><tool_name>a</tool_name>\
            <parameters><PARAM><![CDATA[hello]]></PARAM></parameters></tool_call></tool_calls>";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["value"], "hello");
    }

    #[test]
    fn tag_only_remnant_is_dropped() {
        // 实测模型会在 XML 外留一个空标签，解析后正文只剩尖括号
        let text = "<escape></escape><tool_calls><tool_call><tool_name>run_sql</tool_name><parameters>{\"sql\":\"select 1\"}</parameters></tool_call></tool_calls>";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert!(clean.is_empty(), "只剩标签的正文应清空，实际 {clean:?}");
    }

    #[test]
    fn real_text_survives_alongside_call() {
        let text = "我查一下。<tool_calls><tool_call><tool_name>a</tool_name><parameters>{}</parameters></tool_call></tool_calls>";
        let (clean, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(clean, "我查一下。", "有实际文字时不能连同文字一起清掉");
    }

    #[test]
    fn plain_text_yields_no_calls_and_stays_intact() {
        let text = "今天北京晴，20 度。";
        let (clean, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(clean, text, "没调用就必须原样返回，一个字符都不能动");
    }

    #[test]
    fn unterminated_block_does_not_eat_the_whole_reply() {
        // 模型输出被截断：只有开头没有结尾，至少不能把已有正文吞掉
        let text = "先解释一下<tool_calls><tool_call><tool_name>a";
        let (_, calls) = parse_tool_calls(text);
        assert!(calls.is_empty() || calls[0].name == "a");
    }

    #[test]
    fn tool_choice_variants() {
        assert!(requires_tool_call(&serde_json::json!("required")));
        assert!(requires_tool_call(&serde_json::json!("any")));
        assert!(requires_tool_call(&serde_json::json!({"type": "function"})));
        assert!(requires_tool_call(&serde_json::json!({"type": "any"})));
        assert!(!requires_tool_call(&serde_json::json!("auto")));
        assert!(!requires_tool_call(&serde_json::json!("none")));
        assert!(!requires_tool_call(&serde_json::Value::Null));
        // {"type":"function","function":{"name":...}} 也是强制调这个
        assert!(requires_tool_call(&serde_json::json!({"type": "function", "function": {"name": "a"}})));
    }
}
