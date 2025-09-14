use regex::Regex;
use rquickjs::{Context, Function, Object, Runtime};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct ServerRequest {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub query: HashMap<String, String>,
    pub body: String,
    pub remote_addr: String,
    pub remote_port: u16,
    pub is_https: bool,
    pub protocol: String,
    pub content_length: usize,
    pub content_type: String,
    pub server_user: String,
    pub server_name: String,
    pub server_port: u16,
    pub server_home: String,
    pub server_addr: String,
}

#[derive(Debug)]
struct ScriptBlock {
    content: String,
    start_pos: usize,
    end_pos: usize,
}

pub type JsResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn create_js_context() -> JsResult<(Runtime, Context)> {
    let runtime = Runtime::new()?;
    let context = Context::full(&runtime)?;
    Ok((runtime, context))
}

fn extract_server_scripts(html: &str) -> Vec<ScriptBlock> {
    let re = Regex::new(r#"<script\s+server[^>]*>([\s\S]*?)</script>"#).expect("Invalid regex");

    re.captures_iter(html)
        .map(|cap| {
            let full_match = cap.get(0).unwrap();
            let content = cap.get(1).unwrap().as_str().to_string();

            ScriptBlock {
                content,
                start_pos: full_match.start(),
                end_pos: full_match.end(),
            }
        })
        .collect()
}

fn better_eval_scripts(ctx: &Context, content: &str) -> Result<(), String> {
    ctx.with(|ctx| match ctx.eval::<(), _>(content) {
        Ok(result) => Ok(result),
        Err(rquickjs::Error::Exception) => ctx
            .catch()
            .as_exception()
            .map(|js_error| Err(format!("{}", js_error)))
            .unwrap_or(Err("Unknown JavaScript error".to_string())),
        Err(e) => Err(format!("{}", e)),
    })
}

pub fn inject_server_globals(
    ctx: &Context,
    request: &ServerRequest,
    output_buffer: Arc<std::sync::Mutex<String>>,
) -> JsResult<()> {
    ctx.with(|ctx| {
        let globals = ctx.globals();

        let request_obj = Object::new(ctx.clone())?;
        request_obj.set("method", request.method.clone())?;
        request_obj.set("url", request.url.clone())?;

        let query_obj = Object::new(ctx.clone())?;
        for (k, v) in &request.query {
            query_obj.set(k, v)?;
        }
        request_obj.set("query", query_obj)?;
        request_obj.set("body", request.body.clone())?;

        let headers_obj = Object::new(ctx.clone())?;
        for (k, v) in &request.headers {
            headers_obj.set(k.replace("-", "_"), v)?;
        }
        request_obj.set("headers", headers_obj)?;

        request_obj.set("content_length", request.body.len())?;
        request_obj.set("content_type", request.content_type.clone())?;
        request_obj.set("protocol", request.protocol.clone())?;
        request_obj.set("server_user", request.server_user.clone())?;
        request_obj.set("server_port", request.server_port.clone())?;
        request_obj.set("server_addr", request.server_addr.clone())?;
        request_obj.set("server_name", request.server_name.clone())?;
        request_obj.set("server_home", request.server_home.clone())?;
        request_obj.set("remote_addr", request.remote_addr.clone())?;
        request_obj.set("remote_port", request.remote_port.clone())?;
        request_obj.set("is_https", request.is_https)?;

        globals.set("REQ", request_obj)?;

        let output_clone = output_buffer.clone();
        globals.set(
            "__write",
            Function::new(ctx.clone(), move |content: String| {
                if let Ok(mut buffer) = output_clone.lock() {
                    buffer.push_str(&content);
                }
            }),
        )?;

        globals.set(
            "__log",
            Function::new(ctx.clone(), |args: String| {
                println!("[JS] {}", args);
            }),
        )?;

        Ok::<(), rquickjs::Error>(())
    })?;

    better_eval_scripts(
        ctx,
        r#"
globalThis.console = {
log: (...args) => __log(args.map(String).join(" "))
};
globalThis.console.error = globalThis.console.log;
globalThis.console.warn = globalThis.console.log;
globalThis.console.info = globalThis.console.log;
globalThis.write = (...args) => __write(args.map(String).join(" "));
try { gloablThis.REQ.body = JSON.parse(gloablThis.REQ.body) } catch {};
"#,
    )?;

    Ok(())
}

pub fn execute_server_scripts(html: &str, request: &ServerRequest) -> JsResult<String> {
    let scripts = extract_server_scripts(html);

    if scripts.is_empty() {
        return Ok(html.to_string());
    }

    let (_runtime, ctx) = create_js_context()?;
    let output_buffer = Arc::new(std::sync::Mutex::new(String::new()));

    inject_server_globals(&ctx, request, output_buffer.clone())?;

    let mut result_html = html.to_string();
    let mut offset = 0i32;

    let mut scripts_sorted = scripts;
    scripts_sorted.sort_by_key(|s| s.start_pos);

    for script in scripts_sorted {
        if let Ok(mut buffer) = output_buffer.lock() {
            buffer.clear();
        }

        let execution_result = better_eval_scripts(&ctx, &script.content);

        let output = if let Ok(buffer) = output_buffer.lock() {
            buffer.clone()
        } else {
            String::new()
        };

        let replacement = match execution_result {
            Ok(_) => output,
            Err(e) => {
                eprintln!("JavaScript Error: {}", e);
                format!("<!-- Server JS Error -->",)
            }
        };

        let start = (script.start_pos as i32 + offset) as usize;
        let end = (script.end_pos as i32 + offset) as usize;

        result_html.replace_range(start..end, &replacement);

        offset += replacement.len() as i32 - (script.end_pos - script.start_pos) as i32;
    }

    Ok(result_html)
}
