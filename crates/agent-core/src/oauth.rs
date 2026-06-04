use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    process::Command,
    thread,
    time::{Duration, Instant},
};

const CLIENT_ID: &str = "jucode-cli";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub struct OAuthLoginResult {
    pub web_url: String,
    pub api_url: String,
    pub api_key: String,
    pub models: Vec<OAuthModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthModel {
    pub id: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub reasoning_efforts: Option<Vec<String>>,
}

pub fn login(web_url: &str, api_url: &str) -> Result<OAuthLoginResult, String> {
    let web_url = web_url.trim().trim_end_matches('/').to_string();
    let api_url = api_url.trim().trim_end_matches('/').to_string();
    if web_url.is_empty() {
        return Err("JuCode web URL cannot be empty".to_string());
    }
    if api_url.is_empty() {
        return Err("JuCode API URL cannot be empty".to_string());
    }

    let verifier = random_token(32)?;
    let challenge = pkce_challenge(&verifier);
    let state = random_token(24)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let authorize_url = format!(
        "{}/cli/oauth?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        web_url,
        url_encode(CLIENT_ID),
        url_encode(&redirect_uri),
        url_encode(&challenge),
        url_encode(&state),
    );
    open_browser(&authorize_url)
        .map_err(|error| format!("{error}. Open manually: {authorize_url}"))?;

    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for OAuth callback".to_string());
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(CALLBACK_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|error| error.to_string())?;
    let params = parse_callback_query(&request_line)?;
    let mut stream = reader.into_inner();

    if params.get("state") != Some(&state) {
        write_callback_response(&mut stream, false)?;
        return Err("OAuth state mismatch".to_string());
    }
    let Some(code) = params.get("code").filter(|value| !value.is_empty()) else {
        write_callback_response(&mut stream, false)?;
        return Err("OAuth callback did not include code".to_string());
    };
    write_callback_response(&mut stream, true)?;

    let api_key = exchange_code(&api_url, code, &redirect_uri, &verifier)?;
    let models = fetch_models(&api_url, &api_key).unwrap_or_default();
    Ok(OAuthLoginResult {
        web_url,
        api_url,
        api_key,
        models,
    })
}

fn exchange_code(
    base_url: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<String, String> {
    let url = format!("{}/v1/oauth/cli/token", base_url);
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        }));
    let value = json_response(response)?;
    value
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "OAuth token response did not include api_key".to_string())
}

fn fetch_models(base_url: &str, api_key: &str) -> Result<Vec<OAuthModel>, String> {
    let url = format!("{}/v1/models", base_url);
    let value = json_response(
        ureq::get(&url)
            .set("Authorization", &format!("Bearer {api_key}"))
            .call(),
    )?;
    Ok(parse_models_response(&value))
}

fn parse_models_response(value: &Value) -> Vec<OAuthModel> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_model)
        .collect()
}

fn parse_model(item: &Value) -> Option<OAuthModel> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    Some(OAuthModel {
        id,
        context_window: read_u64_field(item, &["context_window", "context_length"]),
        max_output_tokens: read_u64_field(item, &["max_output_tokens", "max_output"]),
        reasoning_efforts: item
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|values| !values.is_empty()),
    })
}

fn read_u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(Value::as_u64)
}

fn json_response(response: Result<ureq::Response, ureq::Error>) -> Result<Value, String> {
    match response {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|error| error.to_string()),
        Err(ureq::Error::Status(code, response)) => {
            let body = response
                .into_string()
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            Err(format!("JuCode OAuth returned HTTP {code}: {body}"))
        }
        Err(error) => Err(error.to_string()),
    }
}

fn parse_callback_query(request_line: &str) -> Result<HashMap<String, String>, String> {
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "invalid OAuth callback request".to_string())?;
    let query = path
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default();
    Ok(query
        .split('&')
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((url_decode(key), url_decode(value)))
        })
        .collect())
}

fn write_callback_response(stream: &mut impl Write, ok: bool) -> Result<(), String> {
    let body = if ok {
        "JuCode CLI login complete. You can close this tab."
    } else {
        "JuCode CLI login failed. Return to the terminal."
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .map_err(|error| error.to_string())
}

fn random_token(bytes: usize) -> Result<String, String> {
    let mut data = vec![0_u8; bytes];
    getrandom::getrandom(&mut data).map_err(|error| error.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(data))
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn open_browser(url: &str) -> Result<(), String> {
    let status = if cfg!(windows) {
        Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .status()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    }
    .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("failed to open browser: {status}"))
    }
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn url_decode(value: &str) -> String {
    let mut output = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                output.push(hex);
                index += 3;
                continue;
            }
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_metadata_from_models_response() {
        let value = json!({
            "data": [{
                "id": "gpt-5.5",
                "context_window": 1050000,
                "max_output_tokens": 128000,
                "reasoning_efforts": ["low", "medium"]
            }]
        });

        assert_eq!(
            parse_models_response(&value),
            vec![OAuthModel {
                id: "gpt-5.5".to_string(),
                context_window: Some(1_050_000),
                max_output_tokens: Some(128_000),
                reasoning_efforts: Some(vec!["low".to_string(), "medium".to_string()])
            }]
        );
    }

    #[test]
    fn parses_legacy_id_only_models_response() {
        let value = json!({ "data": [{ "id": "gpt-5.4-mini" }] });

        assert_eq!(
            parse_models_response(&value),
            vec![OAuthModel {
                id: "gpt-5.4-mini".to_string(),
                context_window: None,
                max_output_tokens: None,
                reasoning_efforts: None
            }]
        );
    }
}
