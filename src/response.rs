use std::collections::HashMap;

pub struct Response {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    pub fn with_body(mut self, body: Vec<u8>, content_type: &str) -> Self {
        self.headers.insert("Content-Type".to_string(), content_type.to_string());
        self.headers.insert("Content-Length".to_string(), body.len().to_string());
        self.body = body;
        self
    }

    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    pub fn redirect(code: u16, location: &str) -> Self {
        Response::new(code)
            .with_header("Location", location)
            .with_header("Content-Length", "0")
    }

    pub fn error(code: u16, custom_page: Option<Vec<u8>>) -> Self {
        let body = custom_page.unwrap_or_else(|| default_error_page(code).into_bytes());
        Response::new(code).with_body(body, "text/html; charset=utf-8")
    }

    pub fn serialize(&self) -> Vec<u8> {
        let reason = status_reason(self.status);
        let mut output = format!("HTTP/1.1 {} {}\r\n", self.status, reason).into_bytes();

        for (key, value) in &self.headers {
            output.extend_from_slice(format!("{}: {}\r\n", key, value).as_bytes());
        }
        output.extend_from_slice(b"\r\n");
        output.extend_from_slice(&self.body);
        output
    }
}

fn status_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Unknown",
    }
}

pub fn default_error_page(code: u16) -> String {
    let reason = status_reason(code);
    format!(
        "<!DOCTYPE html>\
        <html><head><title>{code} {reason}</title></head>\
        <body><h1>{code} {reason}</h1></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_200() {
        let resp = Response::new(200).with_body(b"hello".to_vec(), "text/plain");
        let bytes = resp.serialize();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 5"));
        assert!(text.ends_with("hello"));
    }

    #[test]
    fn test_redirect() {
        let resp = Response::redirect(301, "/new-location");
        let bytes = resp.serialize();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 301 Moved Permanently\r\n"));
        assert!(text.contains("Location: /new-location"));
    }
}
