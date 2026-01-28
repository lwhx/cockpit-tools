use crate::models::codex::CodexTokens;
use crate::modules::logger;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::io::{ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_ENDPOINT: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const SCOPES: &str = "openid profile email offline_access";
const ORIGINATOR: &str = "codex_vscode";
const OAUTH_CALLBACK_PORT: u16 = 1455;
const OAUTH_PORT_IN_USE_CODE: &str = "CODEX_OAUTH_PORT_IN_USE";

pub fn get_callback_port() -> u16 {
    OAUTH_CALLBACK_PORT
}

/// OAuth 状态存储
struct OAuthState {
    code_verifier: String,
    state: String,
    port: u16,
    tx: Option<oneshot::Sender<String>>,
}

lazy_static::lazy_static! {
    static ref OAUTH_STATE: Arc<Mutex<Option<OAuthState>>> = Arc::new(Mutex::new(None));
}

/// 生成 Base64URL 随机 token（用于 state / code_verifier）
fn generate_base64url_token() -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 生成 PKCE code_challenge
fn generate_code_challenge(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let result = hasher.finalize();
    URL_SAFE_NO_PAD.encode(result)
}

/// 找到可用端口
fn find_available_port() -> Result<u16, String> {
    match TcpListener::bind(("127.0.0.1", OAUTH_CALLBACK_PORT)) {
        Ok(listener) => {
            drop(listener);
            Ok(OAUTH_CALLBACK_PORT)
        }
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            Err(format!("{}:{}", OAUTH_PORT_IN_USE_CODE, OAUTH_CALLBACK_PORT))
        }
        Err(e) => Err(format!("无法绑定端口 {}: {}", OAUTH_CALLBACK_PORT, e)),
    }
}

fn notify_cancel(port: u16) {
    if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
        let _ = stream.write_all(
            b"GET /cancel HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.flush();
    }
}

/// 准备 OAuth URL（返回给前端显示）
pub async fn prepare_oauth_url(app_handle: AppHandle) -> Result<String, String> {
    let port = find_available_port()?;
    let code_verifier = generate_base64url_token();
    let code_challenge = generate_code_challenge(&code_verifier);
    let state = generate_base64url_token();
    
    let redirect_uri = format!("http://localhost:{}/auth/callback", port);
    
    // 构建授权 URL（与 Codex CLI 一致）
    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={}&originator={}",
        AUTH_ENDPOINT,
        CLIENT_ID,
        &redirect_uri,
        urlencoding::encode(SCOPES),
        code_challenge,
        state,
        urlencoding::encode(ORIGINATOR)
    );
    
    // 创建 channel 用于接收回调
    let (tx, _rx) = oneshot::channel::<String>();
    
    // 保存状态
    {
        let mut oauth_state = OAUTH_STATE.lock().unwrap();
        *oauth_state = Some(OAuthState {
            code_verifier,
            state: state.clone(),
            port,
            tx: Some(tx),
        });
    }
    
    // 启动本地 HTTP 服务器
    let app_handle_clone = app_handle.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) = start_callback_server(port, state_clone, app_handle_clone).await {
            logger::log_error(&format!("OAuth 回调服务器错误: {}", e));
        }
    });
    
    logger::log_info(&format!("Codex OAuth URL 已生成, 端口: {}", port));
    
    Ok(auth_url)
}

/// 启动回调服务器
async fn start_callback_server(port: u16, expected_state: String, app_handle: AppHandle) -> Result<(), String> {
    use tiny_http::{Server, Response};
    
    let server = Server::http(format!("127.0.0.1:{}", port))
        .map_err(|e| format!("启动服务器失败: {}", e))?;
    
    logger::log_info(&format!("Codex OAuth 回调服务器启动于端口 {}", port));
    
    // 设置超时 (5分钟)
    let timeout = std::time::Duration::from_secs(300);
    let start = std::time::Instant::now();
    
    loop {
        let should_stop = {
            let oauth_state = OAUTH_STATE.lock().unwrap();
            match oauth_state.as_ref() {
                Some(state) => state.state != expected_state,
                None => true,
            }
        };

        if should_stop {
            logger::log_info("Codex OAuth 已取消或状态已变更，停止回调监听");
            break;
        }

        if start.elapsed() > timeout {
            logger::log_error("OAuth 回调超时");
            break;
        }
        
        // 非阻塞接收请求
        if let Ok(Some(request)) = server.try_recv() {
            let url = request.url().to_string();
            
            if url.starts_with("/auth/callback") {
                // 解析查询参数
                let query = url.split('?').nth(1).unwrap_or("");
                let params: HashMap<_, _> = query
                    .split('&')
                    .filter_map(|pair| {
                        let mut parts = pair.splitn(2, '=');
                        Some((parts.next()?, parts.next().unwrap_or("")))
                    })
                    .collect();
                
                let code = params.get("code").copied().unwrap_or("");
                let state = params.get("state").copied().unwrap_or("");
                
                // 验证 state
                if state != expected_state {
                    let response = Response::from_string("State mismatch")
                        .with_status_code(400);
                    let _ = request.respond(response);
                    continue;
                }
                
                // 返回成功页面
                let html = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>授权成功</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: linear-gradient(135deg, #667eea 0%, #764ba2 100%); }
        .container { text-align: center; color: white; }
        h1 { font-size: 2.5rem; margin-bottom: 1rem; }
        p { font-size: 1.2rem; opacity: 0.9; }
    </style>
</head>
<body>
    <div class="container">
        <h1>✅ 授权成功</h1>
        <p>您可以关闭此窗口并返回应用</p>
    </div>
</body>
</html>"#;
                
                let response = Response::from_string(html)
                    .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap());
                let _ = request.respond(response);
                
                // 发送 code
                let mut oauth_state = OAUTH_STATE.lock().unwrap();
                if let Some(ref mut state_data) = *oauth_state {
                    if let Some(tx) = state_data.tx.take() {
                        let _ = tx.send(code.to_string());
                    }
                }
                
                // 通知前端
                let _ = app_handle.emit("codex-oauth-callback-received", code);
                
                logger::log_info("Codex OAuth 回调已接收");
                break;
            } else if url.starts_with("/cancel") {
                let response = Response::from_string("Login cancelled")
                    .with_status_code(200);
                let _ = request.respond(response);
                let mut oauth_state = OAUTH_STATE.lock().unwrap();
                *oauth_state = None;
                logger::log_info("Codex OAuth 已取消");
                break;
            } else {
                let response = Response::from_string("Not Found")
                    .with_status_code(404);
                let _ = request.respond(response);
            }
        }
        
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    
    Ok(())
}

/// 用授权码换取 Token
pub async fn exchange_code_for_token(code: &str) -> Result<CodexTokens, String> {
    let (code_verifier, port) = {
        let oauth_state = OAUTH_STATE.lock().unwrap();
        let state = oauth_state.as_ref()
            .ok_or("OAuth 状态不存在")?;
        (state.code_verifier.clone(), state.port)
    };
    
    let redirect_uri = format!("http://localhost:{}/auth/callback", port);
    
    let client = reqwest::Client::new();
    
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", &code_verifier),
    ];
    
    logger::log_info(&format!("Codex OAuth 交换 Token, redirect_uri: {}", redirect_uri));
    
    let response = client
        .post(TOKEN_ENDPOINT)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Token 请求失败: {}", e))?;
    
    let status = response.status();
    let body = response.text().await
        .map_err(|e| format!("读取响应失败: {}", e))?;
    
    if !status.is_success() {
        logger::log_error(&format!("Token 交换失败: {} - {}", status, body));
        return Err(format!("Token 交换失败: {}", body));
    }
    
    logger::log_info("Codex OAuth Token 交换成功");
    
    // 解析响应
    let token_response: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("解析 Token 响应失败: {}", e))?;
    
    let id_token = token_response.get("id_token")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 id_token")?
        .to_string();
    
    let access_token = token_response.get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 access_token")?
        .to_string();
    
    let refresh_token = token_response.get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    
    // 清理状态
    {
        let mut oauth_state = OAUTH_STATE.lock().unwrap();
        *oauth_state = None;
    }
    
    Ok(CodexTokens {
        id_token,
        access_token,
        refresh_token,
    })
}

/// 取消 OAuth 流程
pub fn cancel_oauth_flow() {
    let port = {
        let mut oauth_state = OAUTH_STATE.lock().unwrap();
        let port = oauth_state.as_ref().map(|state| state.port).unwrap_or(OAUTH_CALLBACK_PORT);
        *oauth_state = None;
        port
    };
    notify_cancel(port);
    logger::log_info("Codex OAuth 流程已取消");
}

/// 检查 access_token 是否过期
pub fn is_token_expired(access_token: &str) -> bool {
    // 解析 JWT payload
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
        return true; // 格式不正确，视为过期
    }
    
    // Base64URL 解码 payload
    let payload_base64 = parts[1];
    let payload_bytes = match URL_SAFE_NO_PAD.decode(payload_base64) {
        Ok(bytes) => bytes,
        Err(_) => return true,
    };
    
    let payload_str = match String::from_utf8(payload_bytes) {
        Ok(s) => s,
        Err(_) => return true,
    };
    
    // 解析 JSON
    let payload: serde_json::Value = match serde_json::from_str(&payload_str) {
        Ok(v) => v,
        Err(_) => return true,
    };
    
    // 获取 exp 字段
    let exp = match payload.get("exp").and_then(|e| e.as_i64()) {
        Some(e) => e,
        None => return true,
    };
    
    // 比较时间（提前 60 秒视为过期）
    let now = chrono::Utc::now().timestamp();
    exp < now + 60
}

/// 使用 refresh_token 刷新 access_token
pub async fn refresh_access_token(refresh_token: &str) -> Result<CodexTokens, String> {
    let client = reqwest::Client::new();
    
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ];
    
    logger::log_info("Codex Token 刷新中...");
    
    let response = client
        .post(TOKEN_ENDPOINT)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Token 刷新请求失败: {}", e))?;
    
    let status = response.status();
    let body = response.text().await
        .map_err(|e| format!("读取响应失败: {}", e))?;
    
    if !status.is_success() {
        logger::log_error(&format!("Token 刷新失败: {} - {}", status, &body[..body.len().min(200)]));
        return Err(format!("Token 刷新失败: {}", status));
    }
    
    logger::log_info("Codex Token 刷新成功");
    
    // 解析响应
    let token_response: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("解析 Token 响应失败: {}", e))?;
    
    let id_token = token_response.get("id_token")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 id_token")?
        .to_string();
    
    let access_token = token_response.get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 access_token")?
        .to_string();
    
    // refresh_token 可能会返回新的，也可能不返回
    let new_refresh_token = token_response.get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| Some(refresh_token.to_string()));
    
    Ok(CodexTokens {
        id_token,
        access_token,
        refresh_token: new_refresh_token,
    })
}
