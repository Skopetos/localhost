use crate::cgi;
use crate::config::ServerConfig;
use crate::request::{Method, Request};
use crate::response::{self, Response};
use crate::router::{self, RouteMatch};
use crate::session::{self, SessionStore};
use std::io::Read;
use std::path::{Path, PathBuf};

pub enum HandleOutcome {
    Response(Response),
    Cgi(crate::cgi::CgiProcess),
}

pub fn handle(
    req: &Request,
    servers: &[ServerConfig],
    port: u16,
    sessions: &mut SessionStore,
) -> HandleOutcome {
    let host = req.headers.get("host").map(|s| s.as_str()).unwrap_or("");
    let server = router::match_server(servers, host, port);

    let route_match = match router::match_route(server, req) {
        Some(m) => m,
        None => return HandleOutcome::Response(serve_error(404, server, req)),
    };

    dispatch(req, route_match, sessions)
}

fn dispatch(req: &Request, m: RouteMatch, sessions: &mut SessionStore) -> HandleOutcome {
    let route = m.route;

    // Redirect
    if let Some((code, ref location)) = route.redirect {
        return HandleOutcome::Response(Response::redirect(code, location));
    }

    // Method check
    if !route
        .allowed_methods
        .iter()
        .any(|meth| meth.eq_ignore_ascii_case(req.method.as_str()))
    {
        let allowed = route.allowed_methods.join(", ");
        return HandleOutcome::Response(
            Response::new(405)
                .with_header("Allow", &allowed)
                .with_body(
                    response::default_error_page(405).into_bytes(),
                    "text/html; charset=utf-8",
                ),
        );
    }

    // Resolve filesystem path
    let root = match &route.root {
        Some(r) => r.clone(),
        None => {
            return HandleOutcome::Response(Response::error(500, None));
        }
    };

    let path_suffix = req.uri.strip_prefix(&route.path).unwrap_or("");
    let path_suffix = path_suffix.trim_start_matches('/');
    let fs_path = PathBuf::from(&root).join(path_suffix);

    // CGI
    if let Some(ext) = &route.cgi_extension {
        if req.uri.ends_with(ext.as_str()) {
            return run_cgi_handler(&fs_path, req, ext);
        }
    }

    let resp = match req.method {
        Method::Get => handle_get(&fs_path, route, m.server, req, sessions),
        Method::Post => handle_post(&fs_path, route, m.server, req, sessions),
        Method::Delete => handle_delete(&fs_path, m.server, req),
        _ => Response::new(501).with_body(
            response::default_error_page(501).into_bytes(),
            "text/html; charset=utf-8",
        ),
    };
    HandleOutcome::Response(resp)
}

fn handle_get(
    fs_path: &Path,
    route: &crate::config::RouteConfig,
    server: &ServerConfig,
    req: &Request,
    sessions: &mut SessionStore,
) -> Response {
    let mut path = fs_path.to_path_buf();

    if path.is_dir() {
        // Try default file first
        if let Some(ref index) = route.default_file {
            let index_path = path.join(index);
            if index_path.exists() {
                path = index_path;
            } else if route.directory_listing {
                return serve_directory_listing(&path, &req.uri);
            } else {
                return serve_error(403, server, req);
            }
        } else if route.directory_listing {
            return serve_directory_listing(&path, &req.uri);
        } else {
            return serve_error(403, server, req);
        }
    }

    if !path.exists() {
        return serve_error(404, server, req);
    }

    serve_file(&path, sessions, req)
}

fn handle_post(
    _fs_path: &Path,
    route: &crate::config::RouteConfig,
    server: &ServerConfig,
    req: &Request,
    sessions: &mut SessionStore,
) -> Response {
    // File upload
    if let Some(ref upload_dir) = route.upload_dir {
        return handle_upload(req, upload_dir, server);
    }

    // Form body handling — just acknowledge
    let _ = sessions; // may use session in future expansion
    Response::new(200).with_body(b"OK".to_vec(), "text/plain")
}

fn handle_delete(
    fs_path: &Path,
    server: &ServerConfig,
    req: &Request,
) -> Response {
    if !fs_path.exists() {
        return serve_error(404, server, req);
    }
    match std::fs::remove_file(fs_path) {
        Ok(_) => Response::new(204),
        Err(_) => serve_error(403, server, req),
    }
}

fn handle_upload(req: &Request, upload_dir: &str, _server: &ServerConfig) -> Response {
    let content_type = req
        .headers
        .get("content-type")
        .map(|s| s.as_str())
        .unwrap_or("");

    if content_type.contains("multipart/form-data") {
        let boundary = match parse_boundary(content_type) {
            Some(b) => b,
            None => {
                return Response::error(400, None);
            }
        };
        match save_multipart_uploads(&req.body, &boundary, upload_dir) {
            Ok(files) => {
                let msg = format!("Uploaded: {}", files.join(", "));
                return Response::new(201).with_body(msg.into_bytes(), "text/plain");
            }
            Err(e) => {
                eprintln!("Upload error: {}", e);
                return Response::error(500, None);
            }
        }
    }

    Response::error(400, None)
}

fn run_cgi_handler(fs_path: &Path, req: &Request, ext: &str) -> HandleOutcome {
    let script = match fs_path.to_str() {
        Some(s) => s.to_string(),
        None => return HandleOutcome::Response(Response::error(500, None)),
    };

    let binary = match ext.trim_start_matches('.') {
        "py" | "py3" => "/usr/bin/python3",
        "php" => "/usr/bin/php",
        "sh" => "/bin/sh",
        _ => return HandleOutcome::Response(Response::error(500, None)),
    };

    if !Path::new(&script).exists() {
        return HandleOutcome::Response(Response::error(404, None));
    }

    match cgi::spawn_cgi(&script, req, binary) {
        Ok(proc) => HandleOutcome::Cgi(proc),
        Err(e) => {
            eprintln!("CGI error: {}", e);
            HandleOutcome::Response(Response::error(500, None))
        }
    }
}

pub fn cgi_response_to_http(cgi_resp: cgi::CgiResponse) -> Response {
    let mut resp = Response::new(cgi_resp.status);
    for (k, v) in &cgi_resp.headers {
        resp = resp.with_header(k, v);
    }
    let ct = cgi_resp
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "content-type")
        .map(|(_, v)| v.as_str())
        .unwrap_or("text/html")
        .to_string();
    resp.with_body(cgi_resp.body, &ct)
}

fn serve_file(path: &Path, sessions: &mut SessionStore, req: &Request) -> Response {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Response::error(500, None),
    };

    let mut body = Vec::new();
    if file.read_to_end(&mut body).is_err() {
        return Response::error(500, None);
    }

    let ct = mime_type(path);
    let mut resp = Response::new(200).with_body(body, ct);

    // Session cookie
    let session_id = req
        .headers
        .get("cookie")
        .and_then(|c| session::parse_session_id(c));

    let sid = match session_id {
        Some(id) if sessions.get(&id).is_some() => id,
        _ => {
            let new_id = sessions.create_session();
            resp = resp.with_header(
                "Set-Cookie",
                &format!("session_id={}; HttpOnly; Path=/", new_id),
            );
            new_id
        }
    };
    let _ = sid;

    resp
}

fn serve_directory_listing(path: &Path, uri: &str) -> Response {
    let mut entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return Response::error(500, None),
    };

    let mut items = String::new();
    while let Some(Ok(entry)) = entries.next() {
        let name = entry.file_name().to_string_lossy().to_string();
        let href = if uri.ends_with('/') {
            format!("{}{}", uri, name)
        } else {
            format!("{}/{}", uri, name)
        };
        items.push_str(&format!("<li><a href=\"{}\">{}</a></li>\n", href, name));
    }

    let html = format!(
        "<!DOCTYPE html><html><head><title>Index of {uri}</title></head>\
        <body><h1>Index of {uri}</h1><ul>{items}</ul></body></html>"
    );

    Response::new(200).with_body(html.into_bytes(), "text/html; charset=utf-8")
}

fn serve_error(code: u16, server: &ServerConfig, _req: &Request) -> Response {
    let custom = server
        .error_pages
        .get(&code)
        .and_then(|p| std::fs::read(p).ok());
    Response::error(code, custom)
}

fn mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn parse_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';') {
        let part = part.trim();
        if let Some(b) = part.strip_prefix("boundary=") {
            return Some(b.trim_matches('"').to_string());
        }
    }
    None
}

fn save_multipart_uploads(
    body: &[u8],
    boundary: &str,
    upload_dir: &str,
) -> Result<Vec<String>, String> {
    let delimiter = format!("--{}", boundary);
    let delim_bytes = delimiter.as_bytes();
    let mut saved = Vec::new();

    let mut pos = 0;
    while let Some(start) = find_subsequence(&body[pos..], delim_bytes) {
        let abs_start = pos + start + delim_bytes.len();
        if abs_start + 2 > body.len() {
            break;
        }
        // Skip \r\n after boundary
        let part_start = abs_start + 2;

        // Find next boundary
        let next = find_subsequence(&body[part_start..], delim_bytes);
        let part_end = match next {
            Some(n) => part_start + n - 2, // strip trailing \r\n before boundary
            None => break,
        };

        let part = &body[part_start..part_end];
        pos = part_start;

        // Parse part headers
        let header_end = find_subsequence(part, b"\r\n\r\n");
        let header_end = match header_end {
            Some(h) => h,
            None => continue,
        };

        let headers_raw = &part[..header_end];
        let part_body = &part[header_end + 4..];

        let headers_str = match std::str::from_utf8(headers_raw) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let filename = extract_filename(headers_str);
        if let Some(name) = filename {
            let dest = Path::new(upload_dir).join(&name);
            std::fs::create_dir_all(upload_dir)
                .map_err(|e| format!("mkdir {}: {}", upload_dir, e))?;
            std::fs::write(&dest, part_body)
                .map_err(|e| format!("write {}: {}", name, e))?;
            saved.push(name);
        }
    }

    Ok(saved)
}

fn extract_filename(headers: &str) -> Option<String> {
    for line in headers.lines() {
        if line.to_lowercase().starts_with("content-disposition:") {
            for part in line.split(';') {
                let part = part.trim();
                if let Some(v) = part.strip_prefix("filename=") {
                    return Some(v.trim_matches('"').to_string());
                }
            }
        }
    }
    None
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
