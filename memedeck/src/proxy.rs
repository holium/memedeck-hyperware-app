use std::collections::HashMap;

use hyperware_process_lib::http::server::IncomingHttpRequest;
use hyperware_process_lib::http::HeaderValue;
use lol_html::html_content::ContentType;
use lol_html::{element, text, HtmlRewriter, Settings};
use regex::Regex;
use url::Url;

use hyperware_process_lib::{
    get_blob, http::client::send_request_await_response, http::server::send_response,
};

fn replace_domain(original_url: &Url, new_domain: &str) -> anyhow::Result<Url> {
    let mut new_url = Url::parse(new_domain)?;
    new_url.set_path(original_url.path());
    Ok(new_url)
}

fn split_first_path_segment(url: &Url) -> Result<(String, Url), url::ParseError> {
    let mut new_url = url.clone();

    // Get the first segment
    let first_segment = url
        .path_segments()
        .and_then(|mut segments| segments.next().map(|s| s.to_string()))
        .unwrap_or_default();

    // Collect remaining segments
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.skip(1).collect::<Vec<_>>())
        .unwrap_or_default();

    // Create new path from remaining segments
    let new_path = if segments.is_empty() {
        "/"
    } else {
        &format!("/{}", segments.join("/"))
    };

    new_url.set_path(new_path);
    Ok((first_segment, new_url))
}

fn replace_files(input: &str, output: &str) -> anyhow::Result<String> {
    // TODO single quotes?
    let file_ext_regex =
        Regex::new(r#"\\\"\/[^"]*\.(css|js|ttf|woff2|ico|png|svg|jpg|jpeg|webp|html)[^"]*\\\""#)?;

    let replaced = file_ext_regex
        .replace_all(input, |caps: &regex::Captures| {
            let capture = caps[0].to_string();
            let quoteless = capture.replace(r#"\""#, "");
            let news = format!(r#"\"/{}{}\""#, output, quoteless);
            news
        })
        .to_string();

    Ok(replaced)
}

fn replace_urls_css(input: &str, output: &str) -> anyhow::Result<String> {
    let file_ext_regex = Regex::new(r#"url\((\/[^)]+)\)"#)?;

    let replaced = file_ext_regex
        .replace_all(input, |caps: &regex::Captures| {
            let capture = caps[1].to_string();
            let news = format!(r#"url(/{}{})"#, output, capture);
            news
        })
        .to_string();
    Ok(replaced)
}

fn replace_urls_js(input: &str, output: &str) -> anyhow::Result<String> {
    let file_ext_regex = Regex::new(r#"_next/"#)?;

    // if the input contains /static/chunks, replace it with /memedeck:memedeck:memedeck-tester.os//static/chunks
    let replaced = file_ext_regex
        .replace_all(input, |caps: &regex::Captures| {
            let capture = caps[0].to_string();
            let news = format!(r#"memedeck:memedeck:memedeck-tester.os/{}"#, capture);
            news
        })
        .to_string();
    Ok(replaced)
}

fn modify_html(html_bytes: &[u8], prefix: &str) -> anyhow::Result<Vec<u8>> {
    // Ensure prefix is clean (no leading/trailing slashes for consistency)
    let prefix = prefix.trim_matches('/');

    // List of attributes that can contain URLs
    let url_attributes = vec![
        "href",
        "src",
        // "action",
        // "background",
        // "cite",
        // "data",
        // "icon",
        // "longdesc",
        // "manifest",
        // "poster",
        // "profile",
        // "usemap",
        // "classid",
        // "codebase",
        // "archive",
        // "code",
    ];
    //

    // Build a selector for elements with any of these attributes
    let selector: String = url_attributes
        .iter()
        .map(|attr| format!("[{}]", attr))
        .collect::<Vec<String>>()
        .join(",");

    // Output buffer for the rewritten HTML
    let mut output = Vec::new();

    // Create an HTML rewriter with element content handlers
    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                // Handler for elements with URL attributes
                element!("head", move |el| {
                    el.prepend(&mother_script(prefix), ContentType::Html);
                    Ok(())
                }),
                element!(selector, move |el| {
                    for attr in &url_attributes {
                        if let Some(value) = el.get_attribute(attr) {
                            if value.starts_with('/') {
                                let new_value =
                                    format!(r#"/{}/{}"#, prefix, value.trim_start_matches('/'));
                                el.set_attribute(attr, &new_value)?;
                            }
                        }
                    }
                    Ok(())
                }),
                text!("script", |el| {
                    let text_content = el.as_str();
                    // window_shenanigans(text_content);
                    let replaced = replace_files(text_content, prefix)?;
                    el.replace(&replaced, ContentType::Text);

                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |c: &[u8]| output.extend_from_slice(c),
    );

    // Write the input HTML to the rewriter and finalize
    rewriter.write(html_bytes)?;
    rewriter.end()?;

    Ok(output)
}

pub fn run_proxy(
    request: &IncomingHttpRequest,
    web2_url: &str,
    cookie: &str,
) -> anyhow::Result<()> {
    let blob = get_blob().unwrap();
    let body = blob.bytes().to_vec();

    let url = replace_domain(
        &request.url()?,
        format!("{}/memedeck:memedeck:memedeck-tester.os", web2_url).as_str(),
    )?;

    let (first_path_segment, url) = split_first_path_segment(&url)?;
    let mut headers = HashMap::new();
    headers.insert("Cookie".to_string(), cookie.to_string());
    // headers.insert("x-hyperware-app".to_string(), "true".to_string());

    let mut response =
        send_request_await_response(request.method()?, url, Some(headers), 6000, body)?;
    let resheaders = response.headers_mut();
    resheaders.insert("set-cookie", HeaderValue::from_str(cookie)?);

    // DEVS: choose which headers are necessary for the hyperware client
    // don't put them all, that doesn't work
    let content_type = match resheaders.get("content-type") {
        Some(ct) => ct.to_str()?,
        None => "text/html",
    };
    let mime_regex = Regex::new(";.*")?;
    let mime = mime_regex.replace_all(content_type, "").to_string();

    let mut headers = HashMap::new();
    headers.insert("Content-type".to_string(), content_type.to_owned());

    let body = match mime.as_str() {
        "text/html" => {
            let html = modify_html(response.body(), &first_path_segment)?;
            html
        }
        "text/css" => {
            let text = String::from_utf8_lossy(response.body()).to_string();
            let replaced = replace_urls_css(&text, &first_path_segment)?;
            replaced.as_bytes().to_vec()
        }
        "application/javascript" => {
            let text = String::from_utf8_lossy(response.body()).to_string();
            let replaced = replace_urls_js(&text, &first_path_segment)?;
            replaced.as_bytes().to_vec()
        }
        "application/octet-stream" => {
            let text = String::from_utf8_lossy(response.body()).to_string();
            let replaced = replace_urls_js(&text, &first_path_segment)?;
            replaced.as_bytes().to_vec()
        }
        _ => response.body().to_vec(),
    };
    // let body = modify_html(response.body(), &first_path_segment)?;
    send_response(response.status(), Some(headers), body);

    Ok(())
}

fn mother_script(prefix: &str) -> String {
    let script_text = format!(
        r#"
        <script>
            const HYPERWARE_APP_PATH = '{0}';
        </script>
    "#,
        prefix
    );
    script_text
}
