use std::collections::HashMap;

use hyperware_process_lib::http::server::IncomingHttpRequest;
use url::Url;

use hyperware_process_lib::{
    get_blob, http::client::send_request_await_response, http::server::send_response,
};

fn replace_domain(original_url: &Url, new_domain: &str) -> anyhow::Result<Url> {
    let mut new_url = Url::parse(new_domain)?;
    new_url.set_path(original_url.path());
    Ok(new_url)
}

pub fn run_proxy(
    request: &IncomingHttpRequest,
    web2_url: &str,
    cookie: &str,
    package_path: &str,
) -> anyhow::Result<()> {
    let blob = get_blob().unwrap();
    let body = blob.bytes().to_vec();

    let request_url = request.url()?;

    let url = replace_domain(
        &request_url,
        format!("{}/{}", web2_url, package_path).as_str(),
    )?;

    let mut og_headers = request
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
        .collect::<HashMap<String, String>>();

    og_headers.remove("host");

    match send_request_await_response(request.method()?, url, Some(og_headers), 6000, body) {
        Ok(response) => {
            let mut resheaders = response
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
                .collect::<HashMap<String, String>>();

            resheaders.insert("set-cookie".to_string(), cookie.to_string());
            send_response(
                response.status(),
                Some(resheaders),
                response.body().to_vec(),
            );
            return Ok(());
        }
        Err(e) => {
            return Err(e.into());
        }
    }
}
