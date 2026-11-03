use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::read::ZlibDecoder;
use serde_json::{Value, json};
use smol_os::mcp::{
    framebuffer_png, handle_request, loopback_address, post, serve_lines, tools, validate_arguments,
};
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn request(method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
}

#[test]
fn mcp_discovery_has_only_nine_guest_machine_tools() {
    let response = handle_request(
        &request("initialize", json!({"protocolVersion":"2025-03-26"})),
        "http://127.0.0.1:4317",
        "token",
    )
    .unwrap();
    assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
    let response = handle_request(
        &request("tools/list", json!({})),
        "http://127.0.0.1:4317",
        "token",
    )
    .unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 9);
    assert!(
        tools
            .iter()
            .all(|tool| tool["name"].as_str().unwrap().starts_with("machine_"))
    );
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"].as_str().unwrap().contains("shell"))
    );
    for tool in tools {
        assert_eq!(tool["annotations"]["openWorldHint"], false);
        let name = tool["name"].as_str().unwrap();
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            matches!(name, "machine_info" | "machine_screen" | "machine_inspect")
        );
    }
    assert!(
        handle_request(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            "http://127.0.0.1:4317",
            "token"
        )
        .is_none()
    );
    assert_eq!(
        handle_request(
            &request("resources/list", json!({})),
            "http://127.0.0.1:4317",
            "token"
        )
        .unwrap()["error"]["code"],
        -32601
    );
}

#[test]
fn tool_validation_enforces_guest_operation_limits() {
    for (name, args) in [
        ("machine_run", json!({"cycles":2_000_001})),
        ("machine_run", json!({"cycles":1.5})),
        (
            "machine_write",
            json!({"space":"host","address":0,"bytes":[1]}),
        ),
        (
            "machine_write",
            json!({"space":"ram","address":0,"bytes":[256]}),
        ),
        (
            "machine_inspect",
            json!({"space":"ram","address":0,"length":4097}),
        ),
        ("machine_info", json!({"command":"shell"})),
        ("machine_info", json!({"__proto__":{}})),
        ("shell", json!({})),
    ] {
        assert!(
            validate_arguments(name, &args).is_err(),
            "unexpected valid {name}"
        );
    }
    assert!(validate_arguments("machine_run", &json!([])).is_err());
    assert!(validate_arguments("machine_input", &json!({})).is_err());
    assert!(validate_arguments("machine_run", &json!({"cycles":2_000_000})).is_ok());
    assert_eq!(tools().as_array().unwrap().len(), 9);
}

#[test]
fn png_observation_decodes_to_actual_rgb332_pixels() {
    let mut pixels = vec![0u8; 256 * 192];
    pixels[0] = 255;
    pixels[1] = 224;
    let frame = json!({"width":256,"height":192,"pixels":STANDARD.encode(&pixels)});
    let png = framebuffer_png(&frame).unwrap();
    assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 256);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 192);
    assert_eq!(&png[37..41], b"IDAT");
    let size = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
    let mut decoded = Vec::new();
    ZlibDecoder::new(&png[41..41 + size])
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(&decoded[..7], &[0, 255, 255, 255, 255, 0, 0]);
    assert_eq!(decoded.len(), 192 * (256 * 3 + 1));
    assert!(framebuffer_png(&json!({"width":3,"height":192,"pixels":""})).is_err());
    assert!(framebuffer_png(&json!({"width":256,"height":192,"pixels":"AQ=="})).is_err());
}

fn mock_http(
    status: &str,
    body: Value,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, receiver) = mpsc::channel();
    let status = status.to_owned();
    let join = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let count = stream.read(&mut chunk).unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(boundary) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&bytes[..boundary]);
                let size = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                if bytes.len() >= boundary + 4 + size {
                    break;
                }
            }
        }
        sender.send(String::from_utf8(bytes).unwrap()).unwrap();
        let body = body.to_string();
        write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    (url, receiver, join)
}

#[test]
fn local_http_proxy_authenticates_and_preserves_machine_results() {
    let (url, receiver, join) = mock_http("200 OK", json!({"cycles":32,"serial":"A"}));
    let response = post(
        &url,
        "/api/tool",
        &json!({"name":"machine_run","arguments":{"cycles":32}}),
        Some("secret-token"),
    )
    .unwrap();
    assert_eq!(response, json!({"cycles":32,"serial":"A"}));
    let request = receiver.recv().unwrap();
    join.join().unwrap();
    assert!(request.starts_with("POST /api/tool HTTP/1.1\r\n"));
    assert!(request.contains("Authorization: Bearer secret-token\r\n"));
    assert!(request.ends_with("{\"arguments\":{\"cycles\":32},\"name\":\"machine_run\"}"));
    assert!(loopback_address("https://example.com").is_err());
    assert!(loopback_address("http://127.0.0.1:4317@evil").is_err());
    assert!(loopback_address("http://127.0.0.1:4317/evil").is_err());
}

#[test]
fn backend_errors_are_mcp_errors_and_screen_is_image_content() {
    let (url, receiver, join) = mock_http("403 Forbidden", json!({"error":"Machine is sealed"}));
    let result = handle_request(
        &request(
            "tools/call",
            json!({"name":"machine_assemble","arguments":{"source":"halt"}}),
        ),
        &url,
        "token",
    )
    .unwrap();
    receiver.recv().unwrap();
    join.join().unwrap();
    assert_eq!(result["result"]["isError"], true);
    assert!(
        result["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("sealed")
    );
    let (url, receiver, join) = mock_http(
        "200 OK",
        json!({"width":256,"height":192,"pixels":STANDARD.encode(vec![0; 256 * 192])}),
    );
    let result = handle_request(
        &request("tools/call", json!({"name":"machine_screen"})),
        &url,
        "token",
    )
    .unwrap();
    receiver.recv().unwrap();
    join.join().unwrap();
    assert_eq!(result["result"]["isError"], false);
    assert_eq!(result["result"]["content"][0]["type"], "image");
    assert_eq!(result["result"]["content"][0]["mimeType"], "image/png");
}

#[test]
fn stdio_serializes_only_protocol_messages_and_recovers_from_bad_json() {
    let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":1}\n{bad}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
    let mut output = Vec::new();
    serve_lines(
        Cursor::new(input),
        &mut output,
        "http://127.0.0.1:4317",
        "token",
    )
    .unwrap();
    let results: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["result"], json!({}));
    assert_eq!(results[1]["error"]["code"], -32700);
}
