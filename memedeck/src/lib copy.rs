use crate::hyperware::process::sign;

use anyhow;
use base64::{engine::general_purpose, Engine as _};
use hyperware_process_lib::http::client::send_request_await_response;
use hyperware_process_lib::http::server::{send_response, HttpBindingConfig, WsBindingConfig};
use hyperware_process_lib::http::{Method, StatusCode};
use hyperware_process_lib::logging::{info, init_logging, Level};
use hyperware_process_lib::{
    await_message, call_init, get_blob, get_typed_state, homepage, http, kiprintln, set_state,
    Address, Capability, Message, Request,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

mod proxy;

const WEB2_URL: &str = "https://staging.memedeck.xyz";
const WEB2_LOGIN_ENDPOINT: &str = "https://staging-api.memedeck.xyz/v2/auth/hyperware/login";

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

#[derive(Debug, Serialize, Deserialize)]
struct ProxyStateV1 {
    // TODO this should probably be generic and go on login:sys:sys
    pub cookie: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "version")]
enum VersionedState {
    /// State fully stored in memory, persisted using serde_json.
    /// Future state version will use SQLite.
    V1(ProxyStateV1),
}

impl VersionedState {
    fn load() -> Self {
        get_typed_state(|bytes| serde_json::from_slice(bytes))
            .unwrap_or(Self::V1(ProxyStateV1 { cookie: None }))
    }
    fn _save(&self) {
        set_state(&serde_json::to_vec(&self).expect("Failed to serialize state!"));
    }
    fn save_cookie(&self, cookie: String) {
        let ns = Self::V1(ProxyStateV1 {
            cookie: Some(cookie),
        });
        set_state(&serde_json::to_vec(&ns).expect("Failed to serialize state!"));
    }
    fn wipe_cookie(&self) {
        let ns = Self::V1(ProxyStateV1 { cookie: None });
        set_state(&serde_json::to_vec(&ns).expect("Failed to serialize state!"));
    }
    fn get_cookie(&self) -> Option<String> {
        match self {
            Self::V1(ps) => ps.cookie.clone(),
        }
    }
}

call_init!(initialize);
fn initialize(our: Address) {
    init_logging(Level::DEBUG, Level::INFO, None, None, None).unwrap();
    info!("begin");

    homepage::add_to_homepage("Memedeck", Some(ICON), Some("/hyperware/login"), None);

    let mut state = VersionedState::load();
    state.wipe_cookie();

    let mut http_server = http::server::HttpServer::new(5);
    let http_config = HttpBindingConfig::default().secure_subdomain(false);

    http_server.bind_http_path("/", http_config).unwrap();
    http_server
        .bind_ws_path("/", WsBindingConfig::default())
        .unwrap();

    main_loop(&our, &mut state, &mut http_server);
}

fn main_loop(
    our: &Address,
    state: &mut VersionedState,
    http_server: &mut http::server::HttpServer,
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
                let _ = handle_request(our, &source, &body, capabilities, state, http_server);
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
    state: &mut VersionedState,
    http_server: &mut http::server::HttpServer,
) -> anyhow::Result<()> {
    // source node is ALWAYS ourselves since networking is disabled
    if source.process == "http-server:distro:sys" {
        // receive HTTP requests and websocket connection messages from our server
        let server_request = http_server.parse_request(body).unwrap();
        match server_request {
            http::server::HttpServerRequest::Http(request) => {
                handle_page_request(our, state, &request)?;
            }
            // TODO handle websockets
            _ => (),
        };
    };
    Ok(())
}

fn handle_page_request(
    our: &Address,
    state: &mut VersionedState,
    http_request: &http::server::IncomingHttpRequest,
) -> anyhow::Result<()> {
    let cookie = state.get_cookie();

    match cookie {
        Some(cookie) => {
            return proxy::run_proxy(&http_request, WEB2_URL, &cookie);
        }
        None => {
            let cookie = auto_login(our, state)?;

            send_refresh_response(1)?;

            return Ok(());
        }
    }
}

fn auto_login(
    our: &Address,
    state: &mut VersionedState,
    // http_request: &http::server::IncomingHttpRequest,
) -> anyhow::Result<Option<String>> {
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

    let cookie = attempt_login(our, state, message_blob.bytes, signature_blob.bytes)?;
    Ok(cookie)
}

fn attempt_login(
    our: &Address,
    state: &mut VersionedState,
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
    kiprintln!("res: {:?}", res);
    let resbody = res.body();
    let resjson = serde_json::from_slice::<serde_json::Value>(resbody)?;
    kiprintln!("resjson: {:?}", resjson);
    let okres = resjson.get("ok");

    match okres {
        None => {
            kiprintln!("Signature verification failed");
            Err(anyhow::anyhow!("Signature verification failed"))
        }
        Some(_) => {
            let cookie_header = res.headers().get("set-cookie");
            match cookie_header {
                None => {
                    kiprintln!("No cookie found in response");
                    Err(anyhow::anyhow!("No cookie found in response"))
                }
                Some(cookie_value) => {
                    let cookie = cookie_value.to_str()?;
                    kiprintln!("Cookie fetched successfully: {:?}", cookie);
                    state.save_cookie(cookie.to_string());
                    Ok(Some(cookie.to_string()))
                }
            }
        }
    }
}

fn send_json_response<T: serde::Serialize>(status: StatusCode, data: &T) -> anyhow::Result<()> {
    let json_data = serde_json::to_vec(data)?;
    send_response(
        status,
        Some(HashMap::from([(
            String::from("Content-Type"),
            String::from("application/json"),
        )])),
        json_data,
    );
    Ok(())
}

fn get_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn send_refresh_response(delay_seconds: u32) -> anyhow::Result<()> {
    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta http-equiv="refresh" content="1; url=/memedeck:memedeck:memedeck-tester.os/home">
    <title>Redirecting...</title>
</head>
<body>
    <h1>Login Successful</h1>
    <p>You are being redirected to the application...</p>
</body>
</html>"#,
    );

    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "text/html".to_string());

    send_response(StatusCode::OK, Some(headers), html.into_bytes());
    Ok(())
}
