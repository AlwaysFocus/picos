//! Narrow, newline-delimited MCP bridge to the loopback machine service.
//! No shell, filesystem, arbitrary HTTP, or capacity-changing tool is exposed.
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const MAX_REQUEST: usize = 2_000_000;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

fn integer(maximum: u64, minimum: u64) -> Value {
    json!({"type":"integer","minimum":minimum,"maximum":maximum})
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

pub fn tools() -> Value {
    let space = json!({"type":"string","enum":["ram","disk"]});
    let mut catalog = json!([
        {"name":"machine_info","description":"Read machine state, capacity, bootstrap rules and the complete ISA. Call this first.","inputSchema":schema(json!({}), &[])},
        {"name":"machine_screen","description":"Observe actual guest framebuffer pixels as a PNG. No host-rendered text or UI is added.","inputSchema":schema(json!({}), &[])},
        {"name":"machine_assemble","description":"Cross-assemble source and install the binary into guest disk and RAM. Bootstrap only; forbidden after sealing. Consumed cycles are preserved.","inputSchema":schema(json!({"source":{"type":"string","minLength":1,"maxLength":262144}}), &["source"])},
        {"name":"machine_run","description":"Execute a bounded slice of guest instructions and return actual state and serial output.","inputSchema":schema(json!({"cycles":integer(2_000_000,1)}), &["cycles"])},
        {"name":"machine_inspect","description":"Read a bounded range of guest RAM or disk bytes. Framebuffer RAM starts at VIDEO_BASE.","inputSchema":schema(json!({"space":space.clone(),"address":integer(u32::MAX as u64,0),"length":integer(4096,1)}), &["space","address","length"])},
        {"name":"machine_write","description":"Inject guest RAM or disk bytes during bootstrap. Disabled after sealing. No host file access.","inputSchema":schema(json!({"space":space,"address":integer(u32::MAX as u64,0),"bytes":{"type":"array","minItems":1,"maxItems":4096,"items":integer(255,0)}}), &["space","address","bytes"])},
        {"name":"machine_input","description":"Queue keyboard text for guest port 1. The guest must implement its own interpreter.","inputSchema":schema(json!({"text":{"type":"string","maxLength":4096}}), &["text"])},
        {"name":"machine_reset","description":"Reboot the guest disk boot image. Clear RAM/CPU while preserving disk, seal and consumed experiment cycles.","inputSchema":schema(json!({}), &[])},
        {"name":"machine_seal","description":"Permanently disable external assembly and RAM/disk injection for this experiment. Use only when the mission explicitly requests it.","inputSchema":schema(json!({}), &[])}
    ]);
    for tool in catalog.as_array_mut().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let read_only = matches!(name, "machine_info" | "machine_screen" | "machine_inspect");
        let destructive = matches!(
            name,
            "machine_assemble" | "machine_write" | "machine_reset" | "machine_seal"
        );
        tool["annotations"] =
            json!({"readOnlyHint":read_only,"destructiveHint":destructive,"openWorldHint":false});
    }
    catalog
}

pub fn validate_arguments(name: &str, arguments: &Value) -> Result<(), String> {
    let catalog = tools();
    let tool = catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == name)
        .ok_or_else(|| format!("Unknown machine tool: {name}"))?;
    let args = arguments
        .as_object()
        .ok_or("Tool arguments must be an object.")?;
    let spec = &tool["inputSchema"];
    for field in spec["required"].as_array().unwrap() {
        let field = field.as_str().unwrap();
        if !args.contains_key(field) {
            return Err(format!("Missing argument: {field}"));
        }
    }
    for (key, value) in args {
        let rule = spec["properties"]
            .get(key)
            .ok_or_else(|| format!("Unknown argument: {key}"))?;
        let valid = match rule["type"].as_str().unwrap_or("") {
            "integer" => value.as_u64().is_some_and(|number| {
                number >= rule["minimum"].as_u64().unwrap_or(0)
                    && number <= rule["maximum"].as_u64().unwrap_or(u64::MAX)
            }),
            "string" => value.as_str().is_some_and(|text| {
                text.len() >= rule["minLength"].as_u64().unwrap_or(0) as usize
                    && text.len() <= rule["maxLength"].as_u64().unwrap_or(u64::MAX) as usize
                    && rule
                        .get("enum")
                        .is_none_or(|values| values.as_array().unwrap().contains(value))
            }),
            "array" => value.as_array().is_some_and(|items| {
                items.len() >= rule["minItems"].as_u64().unwrap_or(0) as usize
                    && items.len() <= rule["maxItems"].as_u64().unwrap_or(u64::MAX) as usize
                    && items
                        .iter()
                        .all(|item| item.as_u64().is_some_and(|byte| byte <= 255))
            }),
            _ => false,
        };
        if !valid {
            return Err(format!("Invalid {key}."));
        }
    }
    Ok(())
}

pub fn loopback_address(base_url: &str) -> Result<SocketAddr, String> {
    let authority = base_url
        .strip_prefix("http://")
        .ok_or("Bridge must use loopback HTTP.")?
        .trim_end_matches('/');
    let port = authority
        .strip_prefix("127.0.0.1:")
        .ok_or("Bridge must use 127.0.0.1 with an explicit port.")?
        .parse::<u16>()
        .map_err(|_| "Invalid loopback port.")?;
    if port == 0 {
        return Err("Loopback port cannot be zero.".into());
    }
    Ok(SocketAddr::from(([127, 0, 0, 1], port)))
}

/// Bounded HTTP client used only for this process's loopback machine server.
pub fn post(
    base_url: &str,
    path: &str,
    body: &Value,
    token: Option<&str>,
) -> Result<Value, String> {
    let address = loopback_address(base_url)?;
    if !path.starts_with('/') || path.contains(['\r', '\n', ' ']) {
        return Err("Invalid HTTP path.".into());
    }
    if token.is_some_and(|value| value.contains(['\r', '\n'])) {
        return Err("Invalid bridge token.".into());
    }
    let bytes = serde_json::to_vec(body).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_REQUEST {
        return Err("Request too large.".into());
    }
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|error| format!("Machine connection failed: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(40)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let auth = token
        .map(|value| format!("Authorization: Bearer {value}\r\n"))
        .unwrap_or_default();
    let header = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{auth}\r\n",
        bytes.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|_| stream.write_all(&bytes))
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    stream
        .take((MAX_RESPONSE + 1) as u64)
        .read_to_end(&mut response)
        .map_err(|error| format!("Machine response failed: {error}"))?;
    if response.len() > MAX_RESPONSE {
        return Err("Machine response too large.".into());
    }
    let boundary = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or("Malformed HTTP response.")?;
    if boundary > 16_384 {
        return Err("HTTP headers too large.".into());
    }
    let headers =
        std::str::from_utf8(&response[..boundary]).map_err(|_| "Invalid HTTP headers.")?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or("Missing HTTP status.")?;
    let body_bytes = &response[boundary + 4..];
    let mut length = None;
    for line in headers.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("transfer-encoding")
                && !value.trim().eq_ignore_ascii_case("identity")
            {
                return Err("Unsupported chunked machine response.".into());
            }
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| "Invalid Content-Length.")?,
                );
            }
        }
    }
    if length.is_some_and(|expected| expected != body_bytes.len()) {
        return Err("Truncated machine response.".into());
    }
    let value: Value = serde_json::from_slice(body_bytes)
        .map_err(|error| format!("Invalid machine JSON: {error}"))?;
    if !(200..300).contains(&status) {
        return Err(value["error"]
            .as_str()
            .unwrap_or("Machine request failed.")
            .to_owned());
    }
    Ok(value)
}

fn chunk(kind: &[u8; 4], data: &[u8], output: &mut Vec<u8>) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc = 0xffff_ffff_u32;
    for byte in kind.iter().chain(data) {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    output.extend_from_slice(&(crc ^ 0xffff_ffff).to_be_bytes());
}

pub fn framebuffer_png(frame: &Value) -> Result<Vec<u8>, String> {
    if frame["width"] != 256 || frame["height"] != 192 {
        return Err("Invalid framebuffer dimensions.".into());
    }
    let pixels = STANDARD
        .decode(
            frame["pixels"]
                .as_str()
                .ok_or("Missing framebuffer pixels.")?,
        )
        .map_err(|_| "Invalid framebuffer base64.")?;
    if pixels.len() != 256 * 192 {
        return Err("Invalid framebuffer byte count.".into());
    }
    let mut raw = vec![0u8; 192 * (256 * 3 + 1)];
    for y in 0..192 {
        for x in 0..256 {
            let pixel = pixels[y * 256 + x];
            let offset = y * (256 * 3 + 1) + 1 + x * 3;
            raw[offset] = (((pixel >> 5) as u16 * 255 + 3) / 7) as u8;
            raw[offset + 1] = ((((pixel >> 2) & 7) as u16 * 255 + 3) / 7) as u8;
            raw[offset + 2] = (pixel & 3) * 85;
        }
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).map_err(|error| error.to_string())?;
    let compressed = encoder.finish().map_err(|error| error.to_string())?;
    let mut header = Vec::new();
    header.extend_from_slice(&256_u32.to_be_bytes());
    header.extend_from_slice(&192_u32.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
    chunk(b"IHDR", &header, &mut png);
    chunk(b"IDAT", &compressed, &mut png);
    chunk(b"IEND", &[], &mut png);
    Ok(png)
}

pub fn handle_request(request: &Value, base_url: &str, token: &str) -> Option<Value> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let result = |value: Value| Some(json!({"jsonrpc":"2.0","id":id,"result":value}));
    if request["jsonrpc"] != "2.0" || !request["method"].is_string() {
        return Some(
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":"Invalid request."}}),
        );
    }
    request.get("id")?;
    match request["method"].as_str().unwrap() {
        "initialize" => {
            let requested = request["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = if ["2024-11-05", "2025-03-26", "2025-06-18"].contains(&requested) {
                requested
            } else {
                "2025-06-18"
            };
            result(
                json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"smol-machine","version":"0.1.0"},"instructions":"Call machine_info first. All addresses and bytes refer to the resource-limited guest, never the host."}),
            )
        }
        "ping" => result(json!({})),
        "tools/list" => result(json!({"tools":tools()})),
        "tools/call" => {
            let operation = || -> Result<Value, String> {
                let name = request["params"]["name"]
                    .as_str()
                    .ok_or("Missing tool name.")?;
                let empty = json!({});
                let args = request["params"].get("arguments").unwrap_or(&empty);
                validate_arguments(name, args)?;
                let body = post(
                    base_url,
                    "/api/tool",
                    &json!({"name":name,"arguments":args}),
                    Some(token),
                )?;
                if name == "machine_screen" {
                    return Ok(
                        json!({"content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(framebuffer_png(&body)?)}],"isError":false}),
                    );
                }
                Ok(json!({"content":[{"type":"text","text":body.to_string()}],"isError":false}))
            };
            result(operation().unwrap_or_else(
                |error| json!({"content":[{"type":"text","text":error}],"isError":true}),
            ))
        }
        _ => Some(
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found."}}),
        ),
    }
}

pub fn serve_lines<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    base_url: &str,
    token: &str,
) -> Result<(), String> {
    loop {
        let mut line = Vec::new();
        let mut oversized = false;
        let mut eof = false;
        loop {
            let available = input.fill_buf().map_err(|error| error.to_string())?;
            if available.is_empty() {
                eof = true;
                break;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map(|index| index + 1).unwrap_or(available.len());
            if line.len() + take > MAX_REQUEST {
                oversized = true;
            }
            if !oversized {
                line.extend_from_slice(&available[..take]);
            }
            input.consume(take);
            if newline.is_some() {
                break;
            }
        }
        if eof && line.is_empty() && !oversized {
            break;
        }
        let response = if oversized {
            Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Request too large."}}),
            )
        } else if line.iter().all(u8::is_ascii_whitespace) {
            None
        } else {
            match serde_json::from_slice::<Value>(&line) {
                Ok(request) => handle_request(&request, base_url, token),
                Err(_) => Some(
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Invalid JSON."}}),
                ),
            }
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response).map_err(|error| error.to_string())?;
            output
                .write_all(b"\n")
                .and_then(|_| output.flush())
                .map_err(|error| error.to_string())?;
        }
        if eof {
            break;
        }
    }
    Ok(())
}

pub fn run_stdio() -> Result<(), String> {
    let base_url = std::env::var("SMOL_BASE_URL").map_err(|_| "SMOL_BASE_URL is missing.")?;
    loopback_address(&base_url)?;
    let token = std::env::var("SMOL_AGENT_TOKEN").map_err(|_| "SMOL_AGENT_TOKEN is missing.")?;
    if token.is_empty() {
        return Err("SMOL_AGENT_TOKEN is empty.".into());
    }
    serve_lines(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &base_url,
        &token,
    )
}
