use crate::hyperware::process::sign;

use anyhow;
use base64::{engine::general_purpose, Engine as _};
use hyperware_process_lib::http::client::send_request_await_response;
use hyperware_process_lib::http::server::{send_response, HttpBindingConfig, WsBindingConfig};
use hyperware_process_lib::http::{Method, StatusCode};
use hyperware_process_lib::logging::{init_logging, Level};
use hyperware_process_lib::{
    await_message, call_init, get_blob, homepage, http, kiprintln, Address, Capability, Message,
    Request,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

mod proxy;

const WEB2_URL: &str = "https://hyperware.memedeck.xyz";
const WEB2_LOGIN_ENDPOINT: &str = "https://api.memedeck.xyz/v2/auth/hyperware/login";
const PACKAGE_PATH: &str = "/app:memedeck:meme-deck.os";

// const WEB2_URL: &str = "http://localhost:3000";
// const WEB2_LOGIN_ENDPOINT: &str = "http://localhost:8080/v2/auth/hyperware/login";

const WEB2_LOGIN_NONCE: &str = "951f64b8-5905-47f8-b12c-3ca8f53119f2";

wit_bindgen::generate!({
    path: "target/wit",
    world: "memedeck-template-dot-os-v0",
    generate_unused_types: true,
    additional_derives: [PartialEq, serde::Deserialize, serde::Serialize, process_macros::SerdeJsonInto],
});

#[derive(Debug, Serialize, Deserialize)]
enum FrontendRequest {
    Sign,
    CheckCookie,
    Logout,
    Debug(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct LoginMessage {
    pub site: String,
    pub time: u64,
    pub nonce: Option<String>,
}

const ICON: &str = include_str!("icon");

call_init!(initialize);
fn initialize(our: Address) {
    init_logging(Level::DEBUG, Level::INFO, None, None, None).unwrap();
    kiprintln!("begin");

    homepage::add_to_homepage("Memedeck", Some(ICON), Some("/home"), None);

    let mut cookie = None;

    let mut http_server = http::server::HttpServer::new(5);
    let http_config = HttpBindingConfig::default().secure_subdomain(false);

    http_server.bind_http_path("/", http_config).unwrap();
    http_server
        .bind_ws_path("/", WsBindingConfig::default())
        .unwrap();

    main_loop(&our, &mut http_server, &mut cookie);
}

fn main_loop(
    our: &Address,
    http_server: &mut http::server::HttpServer,
    cookie: &mut Option<String>,
) {
    loop {
        match await_message() {
            Err(_send_error) => {
                // ignore send errors, local-only process
                continue;
            }
            Ok(Message::Request {
                source,
                body,
                capabilities,
                ..
            }) => {
                // ignore messages from other nodes -- technically superfluous check
                // since manifest does not acquire networking capability
                if source.node() != our.node {
                    continue;
                }
                let _ = handle_request(our, &source, &body, capabilities, http_server, cookie);
            }
            _ => continue, // ignore responses
        }
    }
}

fn handle_request(
    our: &Address,
    source: &Address,
    body: &[u8],
    _capabilities: Vec<Capability>,
    http_server: &mut http::server::HttpServer,
    cookie: &mut Option<String>,
) -> anyhow::Result<()> {
    // source node is ALWAYS ourselves since networking is disabled
    if source.process == "http-server:distro:sys" {
        // receive HTTP requests and websocket connection messages from our server
        let server_request = http_server.parse_request(body).unwrap();
        match server_request {
            http::server::HttpServerRequest::Http(request) => {
                handle_page_request(our, &request, cookie)?;
            }
            // TODO handle websockets
            _ => (),
        };
    };
    Ok(())
}

fn handle_page_request(
    our: &Address,
    http_request: &http::server::IncomingHttpRequest,
    cookie: &mut Option<String>,
) -> anyhow::Result<()> {
    match cookie {
        Some(cookie) => {
            return proxy::run_proxy(&http_request, WEB2_URL, &cookie, PACKAGE_PATH);
        }
        None => {
            let new_cookie = auto_login(our)?;
            *cookie = new_cookie;

            send_refresh_response(1, cookie.clone().unwrap())?;
            return Ok(());
        }
    }
}

fn auto_login(our: &Address) -> anyhow::Result<Option<String>> {
    let target = Address::new(our.node(), ("sign", "sign", "sys"));
    let body = LoginMessage {
        site: WEB2_URL.to_string(),
        nonce: Some(WEB2_LOGIN_NONCE.to_string()),
        time: get_now(),
    };
    let body_bytes = serde_json::to_vec(&body)?;

    let request_result = Request::to(target.clone())
        .blob_bytes(body_bytes.clone())
        .body(sign::Request::NetKeySign)
        .send_and_await_response(10)?;

    if request_result.is_err() {
        return Err(anyhow::anyhow!("Failed to send request"));
    }

    let signature_blob = get_blob().unwrap();
    let _ = Request::to(target.clone())
        .blob_bytes(body_bytes.clone())
        .body(sign::Request::NetKeyVerify(sign::NetKeyVerifyRequest {
            node: our.node().to_string(),
            signature: signature_blob.clone().bytes,
        }))
        .send_and_await_response(10)??;

    let _ = Request::to(target)
        .blob_bytes(body_bytes)
        .body(sign::Request::NetKeyMakeMessage)
        .send_and_await_response(10)??;
    let message_blob = get_blob().unwrap();

    let new_cookie = attempt_login(our, message_blob.bytes, signature_blob.bytes)?;
    Ok(new_cookie)
}

fn attempt_login(
    our: &Address,
    message: Vec<u8>,
    signature: Vec<u8>,
    //signature_response: SignResponse,
) -> anyhow::Result<Option<String>> {
    kiprintln!("attempt_login");
    let mut json_headers = HashMap::new();

    json_headers.insert("Content-Type".to_string(), "application/json".to_string());
    let node = our.node();

    // Convert message to UTF-8 string if possible, otherwise use base64
    let message_str = match String::from_utf8(message.clone()) {
        Ok(s) => s,
        Err(_) => general_purpose::STANDARD.encode(&message),
    };

    // Encode binary signature as base64 instead of trying to convert it to UTF-8
    let signature_base64 = general_purpose::STANDARD.encode(signature);

    let json = json!({"node": node, "message": message_str, "signature": signature_base64});

    let json_bytes = serde_json::to_vec(&json)?;
    let url = Url::parse(WEB2_LOGIN_ENDPOINT).unwrap();

    let res = match send_request_await_response(
        Method::POST,
        url,
        Some(json_headers),
        5000,
        json_bytes,
    ) {
        Ok(res) => res,
        Err(e) => {
            kiprintln!("Failed to send request: {:?}", e);
            return Err(anyhow::anyhow!("Failed to send request"));
        }
    };
    let resbody = res.body();
    let resjson = serde_json::from_slice::<serde_json::Value>(resbody)?;
    kiprintln!("resjson: {:?}", resjson);
    let res_token = resjson.get("cookie");

    match res_token {
        None => {
            kiprintln!("Signature verification failed");
            Err(anyhow::anyhow!("Signature verification failed"))
        }
        Some(cookie_value) => {
            let cookie = format!(
                "hyperware_token={}; path=/;",
                serde_json::from_value::<String>(cookie_value.clone())?
            );
            kiprintln!("Cookie fetched successfully: {:?}", cookie);
            Ok(Some(cookie))
        }
    }
}

fn get_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn send_refresh_response(delay_seconds: u32, cookie: String) -> anyhow::Result<()> {
    // Get our address to construct a proper path
    let home_path = format!("{}/home", PACKAGE_PATH); // Use the same path defined in initialize()

    let html = format!(
        r#"<!DOCTYPE html>
        <html>
        <head>
            <meta http-equiv="refresh" content="{}; url={}">
            <title>Redirecting...</title>
            <style>
                body {{
                    background-color: #000000;
                    color: #e0e0e0;
                    font-family: Arial, sans-serif;
                    display: flex;
                    flex-direction: column;
                    align-items: center;
                    justify-content: center;
                    height: 100vh;
                    margin: 0;
                    padding: 0;
                }}
                
                h1 {{
                    color: #ffffff;
                    margin-bottom: 20px;
                }}
                
                p {{
                    margin-bottom: 30px;
                }}
                
                .spinner {{
                    width: 50px;
                    height: 50px;
                    border: 5px solid rgba(255, 255, 255, 0.3);
                    border-radius: 50%;
                    border-top-color: #2086FF;
                    animation: spin 1s ease-in-out infinite;
                    margin-bottom: 20px;
                }}
                
                @keyframes spin {{
                    to {{
                        transform: rotate(360deg);
                    }}
                }}
                
                .container {{
                    text-align: center;
                }}
            </style>
        </head>
        <body>
            <div class="container">
                <div class="spinner"></div>
            </div>
        </body>
        </html>"#,
        delay_seconds, home_path
    );

    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "text/html".to_string());
    headers.insert("Set-Cookie".to_string(), cookie);

    Ok(send_response(
        StatusCode::FOUND,
        Some(headers),
        html.into_bytes(),
    ))
}
