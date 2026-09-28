// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A read-only HTTP surface for retained campaign bundles.
//!
//! [`serve_campaigns`] binds one local address and answers GET requests for
//! the named campaigns' retained evidence: the versioned result and replay
//! plan, a rendered markdown report, and the serial logs the result
//! references. Every other method is refused, nothing is ever written, and
//! serial routes cannot escape the bundle's `serial/` directory.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// One served campaign: its display name and its bundle directory.
struct ServedCampaign {
    name: String,
    root: PathBuf,
}

/// Serve retained campaign bundles over local read-only HTTP. Blocks while
/// connections arrive; the process ends when the caller interrupts it.
pub fn serve_campaigns(sources: &[PathBuf], address: &str) -> Result<(), String> {
    let campaigns = collect_campaigns(sources)?;
    let listener =
        TcpListener::bind(address).map_err(|error| format!("cannot bind {address}: {error}"))?;
    eprintln!(
        "theseus serving {} campaign(s) at http://{address}",
        campaigns.len()
    );
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = handle_connection(&mut stream, &campaigns);
    }
    Ok(())
}

fn collect_campaigns(sources: &[PathBuf]) -> Result<Vec<ServedCampaign>, String> {
    if sources.is_empty() {
        return Err("serve needs at least one campaign bundle".to_owned());
    }
    let mut campaigns = Vec::with_capacity(sources.len());
    let mut names = std::collections::BTreeSet::new();
    for source in sources {
        let root = std::fs::canonicalize(source)
            .map_err(|error| format!("{}: {error}", source.display()))?;
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{}: bundle has no directory name", root.display()))?;
        if !names.insert(name.clone()) {
            return Err(format!(
                "two bundles share the name {name}; serve campaigns with distinct directory names"
            ));
        }
        campaigns.push(ServedCampaign { name, root });
    }
    Ok(campaigns)
}

fn handle_connection(stream: &mut TcpStream, campaigns: &[ServedCampaign]) -> std::io::Result<()> {
    let request = read_request(stream)?;
    let Some((method, path)) = parse_request(&request) else {
        return write_response(
            stream,
            400,
            "text/plain; charset=utf-8",
            b"malformed request",
        );
    };
    if method != "GET" {
        return write_response(
            stream,
            405,
            "text/plain; charset=utf-8",
            b"method not allowed; this surface is read-only",
        );
    }
    let (status, content_type, body) = route(campaigns, &path);
    write_response(stream, status, content_type, &body)
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk)?;
        buffer.extend_from_slice(&chunk[..read]);
        if read == 0 || buffer.len() > 8192 || buffer.windows(4).any(|window| window == b"\r\n\r\n")
        {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn parse_request(request: &str) -> Option<(String, String)> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    Some((method, path))
}

fn route(campaigns: &[ServedCampaign], path: &str) -> (u16, &'static str, Vec<u8>) {
    let path = path.split('?').next().unwrap_or("/");
    if path == "/" || path.is_empty() {
        return (200, "text/html; charset=utf-8", index_page(campaigns));
    }
    let trimmed = path.trim_start_matches('/');
    let (name, rest) = match trimmed.split_once('/') {
        Some((name, rest)) => (name, rest),
        None => (trimmed, ""),
    };
    let Some(campaign) = campaigns.iter().find(|campaign| campaign.name == name) else {
        return not_found();
    };
    if rest == "result" {
        return read_file(
            &campaign.root.join("campaign-result.json"),
            "application/json",
        );
    }
    if rest == "plan" {
        return read_file(&campaign.root.join("replay-plan.json"), "application/json");
    }
    if rest == "report" {
        return match crate::report::report_text(
            &campaign.root,
            crate::report::ReportFormat::Markdown,
        ) {
            Ok(markdown) => (200, "text/markdown; charset=utf-8", markdown.into_bytes()),
            Err(_) => (
                500,
                "text/plain; charset=utf-8",
                b"report rendering failed".to_vec(),
            ),
        };
    }
    if let Some(relative) = rest.strip_prefix("serial/") {
        return serve_serial(&campaign.root, relative);
    }
    not_found()
}

fn serve_serial(root: &Path, relative: &str) -> (u16, &'static str, Vec<u8>) {
    if relative
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return not_found();
    }
    read_file(
        &root.join("serial").join(relative),
        "text/plain; charset=utf-8",
    )
}

fn read_file(path: &Path, content_type: &'static str) -> (u16, &'static str, Vec<u8>) {
    match std::fs::read(path) {
        Ok(bytes) => (200, content_type, bytes),
        Err(_) => not_found(),
    }
}

fn not_found() -> (u16, &'static str, Vec<u8>) {
    (404, "text/plain; charset=utf-8", b"not found".to_vec())
}

fn index_page(campaigns: &[ServedCampaign]) -> Vec<u8> {
    let mut page = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>theseus campaigns</title></head><body><h1>Retained campaigns</h1><ul>",
    );
    for campaign in campaigns {
        let name = escape_html(&campaign.name);
        page.push_str(&format!(
            "<li><a href=\"/{name}/report\">{name}</a> · <a href=\"/{name}/result\">result</a> · <a href=\"/{name}/plan\">plan</a></li>"
        ));
    }
    page.push_str("</ul></body></html>");
    page.into_bytes()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn write_bundle(directory: &Path) -> PathBuf {
        let bundle = directory.join("campaign");
        fs::create_dir_all(bundle.join("serial")).unwrap();
        fs::write(
            bundle.join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","campaign":{"operations":[{"name":"calculate"}]}}"#,
        )
        .unwrap();
        fs::write(
            bundle.join("campaign-result.json"),
            r#"{"format":"theseus-compose-campaign-result-v1","status":"failed","driver":"chooser","guidance":"unified","structured_choice_decisions":1,"runs":[{"index":0,"operations":["calculate[mode-1]"],"status":"failed","structured_choices":{"chooser":[{"ordinal":0,"name":"mode","upper_exclusive":2,"selected":1}]}}]}"#,
        )
        .unwrap();
        fs::write(bundle.join("serial").join("1.log"), b"ready\n").unwrap();
        bundle
    }

    fn exchange(address: &str, request: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status: u16 = response
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_default();
        let content_type = response
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-type"))
            .map(|line| line.splitn(2, ": ").nth(1).unwrap_or_default().to_owned())
            .unwrap_or_default();
        let body = response
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        (status, content_type, body)
    }

    #[test]
    fn serves_evidence_routes_and_refuses_everything_else() {
        let directory = tempfile::tempdir().unwrap();
        write_bundle(directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let campaigns = collect_campaigns(&[directory.path().join("campaign")]).unwrap();
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok(mut stream) => {
                        let _ = handle_connection(&mut stream.0, &campaigns);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let (status, content_type, body) =
            exchange(&address, "GET /campaign/result HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(
            body.contains("theseus-compose-campaign-result-v1"),
            "{body}"
        );

        let (status, content_type, body) =
            exchange(&address, "GET /campaign/plan HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("theseus-compose-plan-v1"), "{body}");

        let (status, content_type, body) =
            exchange(&address, "GET /campaign/report HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/markdown; charset=utf-8");
        assert!(body.contains("1 structured choices"), "{body}");

        let (status, content_type, body) = exchange(
            &address,
            "GET /campaign/serial/1.log HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert_eq!(body, "ready\n");

        let (status, ..) = exchange(
            &address,
            "GET /campaign/serial/../campaign-result.json HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, ..) = exchange(
            &address,
            "POST /campaign/result HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 405);

        let (status, ..) = exchange(&address, "GET /nope/result HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 404);

        let (status, content_type, body) = exchange(&address, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert!(body.contains("/campaign/report"), "{body}");

        running.store(false, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[test]
    fn serve_rejects_empty_and_duplicate_sources() {
        assert!(collect_campaigns(&[]).is_err());
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("dupe");
        let right = directory.path().join("nested");
        fs::create_dir_all(&left).unwrap();
        fs::create_dir_all(&right).unwrap();
        assert!(collect_campaigns(&[left.clone(), right.join("dupe")]).is_err());
        let served = collect_campaigns(&[left]).unwrap();
        assert_eq!(served[0].name, "dupe");
    }
}
