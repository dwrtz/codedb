use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::CodeDb;
use crate::workspace::{
    WorkspaceRequest, WorkspaceResponse, execute_workspace_request, workspace_response_json,
};

pub fn serve_workspace(db_path: impl AsRef<Path>, addr: &str) -> Result<()> {
    let db_path = db_path.as_ref().to_path_buf();
    let listener = TcpListener::bind(addr).with_context(|| format!("failed to bind {addr}"))?;
    println!("serving workspace {}", listener.local_addr()?);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(err) = handle_connection(stream, &db_path) {
                    eprintln!("workspace request failed: {err:#}");
                }
            }
            Err(err) => eprintln!("workspace connection failed: {err}"),
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, db_path: &Path) -> Result<()> {
    let request = read_http_request(&mut stream)?;
    let response = match request {
        HttpRequest { method, path, body } if method == "POST" && path == "/" => {
            match serde_json::from_slice::<WorkspaceRequest>(&body) {
                Ok(request) => {
                    let mut db = CodeDb::open(db_path)
                        .with_context(|| format!("failed to open {}", db_path.display()))?;
                    execute_workspace_request(&mut db, request)
                }
                Err(err) => WorkspaceResponse::error(
                    "invalid_request",
                    format!("request body must be a workspace JSON object: {err}"),
                    None,
                    None,
                ),
            }
        }
        HttpRequest { method, path, .. } => WorkspaceResponse::error(
            "invalid_request",
            format!("unsupported HTTP request {method} {path}; expected POST /"),
            None,
            None,
        ),
    };
    write_http_json_response(&mut stream, 200, &workspace_response_json(&response)?)?;
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let (method, path, body) = {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        if request_line.trim().is_empty() {
            bail!("empty HTTP request");
        }
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| anyhow!("missing HTTP method"))?
            .to_string();
        let path = parts
            .next()
            .ok_or_else(|| anyhow!("missing HTTP path"))?
            .to_string();

        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value
                    .trim()
                    .parse::<usize>()
                    .context("invalid Content-Length")?;
            }
        }

        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body)?;
        (method, path, body)
    };

    Ok(HttpRequest { method, path, body })
}

fn write_http_json_response(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    stream.flush()?;
    Ok(())
}
