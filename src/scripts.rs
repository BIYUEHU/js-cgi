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

#[derive(Default, Debug)]
pub struct ResponseState {
    pub status: Option<u16>,
    pub headers: HashMap<String, String>,
    pub cookies: HashMap<String, String>,
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
    resp_state: Arc<std::sync::Mutex<ResponseState>>,
    session_store: Arc<std::sync::Mutex<HashMap<String, HashMap<String, String>>>>,
) -> JsResult<()> {
    ctx.with(|ctx| {
        let globals = ctx.globals();

        // === [REQ] 完整的请求对象注入 (包含所有字段) ===
        let req_obj = Object::new(ctx.clone())?;
        req_obj.set("method", request.method.clone())?;
        req_obj.set("url", request.url.clone())?;
        req_obj.set("body", request.body.clone())?;
        req_obj.set("remote_addr", request.remote_addr.clone())?;
        req_obj.set("remote_port", request.remote_port.clone())?;
        req_obj.set("server_addr", request.server_addr.clone())?;
        req_obj.set("server_port", request.server_port.clone())?;
        req_obj.set("server_name", request.server_name.clone())?;
        req_obj.set("server_user", request.server_user.clone())?;
        req_obj.set("server_home", request.server_home.clone())?;
        req_obj.set("protocol", request.protocol.clone())?;
        req_obj.set("content_type", request.content_type.clone())?;
        req_obj.set("content_length", request.body.len())?;
        req_obj.set("is_https", request.is_https)?;

        // Query 处理
        let query_obj = Object::new(ctx.clone())?;
        for (k, v) in &request.query {
            query_obj.set(k, v)?;
        }
        req_obj.set("query", query_obj)?;

        // Headers 处理 (驼峰风格与下划线兼容)
        let headers_obj = Object::new(ctx.clone())?;
        for (k, v) in &request.headers {
            headers_obj.set(k.replace("-", "_"), v)?;
        }
        req_obj.set("headers", headers_obj)?;
        globals.set("REQ", req_obj)?;

        // === [RES] 响应控制 API ===
        let res_obj = Object::new(ctx.clone())?;
        let rs_status = resp_state.clone();
        res_obj.set(
            "status",
            Function::new(ctx.clone(), move |code: u16| {
                if let Ok(mut s) = rs_status.lock() {
                    s.status = Some(code);
                }
            }),
        )?;
        let rs_headers = resp_state.clone();
        res_obj.set(
            "setHeader",
            Function::new(ctx.clone(), move |k: String, v: String| {
                if let Ok(mut s) = rs_headers.lock() {
                    s.headers.insert(k, v);
                }
            }),
        )?;
        globals.set("RES", res_obj)?;

        // === [fs] 文件系统 API ===
        let fs_obj = Object::new(ctx.clone())?;
        fs_obj.set(
            "readFile",
            Function::new(ctx.clone(), |p: String| std::fs::read_to_string(p).ok()),
        )?;
        fs_obj.set(
            "writeFile",
            Function::new(ctx.clone(), |p: String, c: String| {
                std::fs::write(p, c).is_ok()
            }),
        )?;
        fs_obj.set(
            "exists",
            Function::new(ctx.clone(), |p: String| std::path::Path::new(&p).exists()),
        )?;
        fs_obj.set(
            "remove",
            Function::new(ctx.clone(), |p: String| std::fs::remove_file(p).is_ok()),
        )?;
        fs_obj.set(
            "mkdir",
            Function::new(ctx.clone(), |p: String| std::fs::create_dir_all(p).is_ok()),
        )?;
        fs_obj.set(
            "readdir",
            Function::new(ctx.clone(), |p: String| {
                std::fs::read_dir(p).ok().map(|rd| {
                    rd.filter_map(|e| {
                        e.ok()
                            .map(|ent| ent.file_name().to_string_lossy().to_string())
                    })
                    .collect::<Vec<_>>()
                })
            }),
        )?;
        globals.set("fs", fs_obj)?;

        // --- [path] 路径处理 API (已修正 E0515) ---
        let path_obj = Object::new(ctx.clone())?;

        // join: 合并路径并返回拥有的 String
        path_obj.set(
            "join",
            Function::new(ctx.clone(), |a: String, b: String| {
                std::path::Path::new(&a)
                    .join(b)
                    .to_string_lossy()
                    .into_owned()
            }),
        )?;

        // dirname: 获取父目录并返回拥有的 String
        path_obj.set(
            "dirname",
            Function::new(ctx.clone(), |p: String| {
                std::path::Path::new(&p)
                    .parent()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
        )?;

        // basename: 获取文件名并返回拥有的 String
        path_obj.set(
            "basename",
            Function::new(ctx.clone(), |p: String| {
                std::path::Path::new(&p)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
        )?;

        // extname: 获取扩展名并返回拥有的 String
        path_obj.set(
            "extname",
            Function::new(ctx.clone(), |p: String| {
                std::path::Path::new(&p)
                    .extension()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
        )?;

        globals.set("path", path_obj)?;

        // === [COOKIE] Cookie 读写 ===
        let cookie_obj = Object::new(ctx.clone())?;
        let raw_cookie = request.headers.get("cookie").cloned().unwrap_or_default();
        let parsed_cookies: HashMap<String, String> = raw_cookie
            .split(';')
            .filter_map(|s| {
                let mut p = s.splitn(2, '=');
                Some((p.next()?.trim().into(), p.next()?.trim().into()))
            })
            .collect();
        let rs_cookie = resp_state.clone();
        cookie_obj.set(
            "set",
            Function::new(ctx.clone(), move |k: String, v: String| {
                if let Ok(mut s) = rs_cookie.lock() {
                    s.cookies.insert(k, v);
                }
            }),
        )?;
        let parsed_cookies_clone = parsed_cookies.clone();
        cookie_obj.set(
            "get",
            Function::new(ctx.clone(), move |k: String| {
                parsed_cookies_clone.get(&k).cloned()
            }),
        )?;
        globals.set("COOKIE", cookie_obj)?;

        // === [SESSION] 会话管理 ===
        let sid = parsed_cookies
            .get("sid")
            .cloned()
            .unwrap_or_else(|| "default_sid".into());
        let session_obj = Object::new(ctx.clone())?;
        let s_store_get = session_store.clone();
        let sid_get = sid.clone();
        session_obj.set(
            "get",
            Function::new(ctx.clone(), move |k: String| {
                s_store_get
                    .lock()
                    .unwrap()
                    .get(&sid_get)
                    .and_then(|m| m.get(&k))
                    .cloned()
            }),
        )?;
        let s_store_set = session_store.clone();
        let sid_set = sid.clone();
        session_obj.set(
            "set",
            Function::new(ctx.clone(), move |k: String, v: String| {
                s_store_set
                    .lock()
                    .unwrap()
                    .entry(sid_set.clone())
                    .or_default()
                    .insert(k, v);
            }),
        )?;
        globals.set("SESSION", session_obj)?;

        // === 基础回调 ===
        let out_clone = output_buffer.clone();
        globals.set(
            "__write",
            Function::new(ctx.clone(), move |c: String| {
                if let Ok(mut b) = out_clone.lock() {
                    b.push_str(&c);
                }
            }),
        )?;
        globals.set(
            "__log",
            Function::new(ctx.clone(), |s: String| println!("[JS LOG] {}", s)),
        )?;

        Ok::<(), rquickjs::Error>(())
    })?;

    // === JS 环境初始化脚本 (你的原始逻辑完整版) ===
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
        try { globalThis.REQ.body = JSON.parse(globalThis.REQ.body); } catch(e) {}
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

    let resp_state = Arc::new(std::sync::Mutex::new(ResponseState::default()));
    let session_store = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    inject_server_globals(
        &ctx,
        request,
        output_buffer.clone(),
        resp_state,
        session_store,
    )?;

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
