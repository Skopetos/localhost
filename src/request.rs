use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Method {
    Get,
    Post,
    Delete,
    Other(String),
}

impl Method {
    pub fn from_str(s: &str) -> Self {
        match s {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "DELETE" => Method::Delete,
            other => Method::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
            Method::Other(s) => s.as_str(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub uri: String,
    pub version: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub query_string: String,
}

#[derive(Debug)]
pub enum ParseError {
    Invalid(String),
    TooLarge,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Invalid(s) => write!(f, "Invalid request: {}", s),
            ParseError::TooLarge => write!(f, "Request entity too large"),
        }
    }
}

pub struct RequestParser {
    buffer: Vec<u8>,
    max_body_size: usize,
}

impl RequestParser {
    pub fn new(max_body_size: usize) -> Self {
        RequestParser {
            buffer: Vec::new(),
            max_body_size,
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    pub fn try_parse(&mut self) -> Result<Option<Request>, ParseError> {
        // Find the end of headers (\r\n\r\n)
        let header_end = match find_subsequence(&self.buffer, b"\r\n\r\n") {
            Some(pos) => pos,
            None => return Ok(None), // headers not complete yet
        };

        let header_section = &self.buffer[..header_end];
        let mut header_lines = header_section.split(|&b| b == b'\n');

        // Parse request line
        let request_line = header_lines
            .next()
            .ok_or_else(|| ParseError::Invalid("Empty request".to_string()))?;
        let request_line = strip_cr(request_line);
        let request_line =
            std::str::from_utf8(request_line).map_err(|_| ParseError::Invalid("Non-UTF8 request line".to_string()))?;

        let mut parts = request_line.splitn(3, ' ');
        let method_str = parts
            .next()
            .ok_or_else(|| ParseError::Invalid("Missing method".to_string()))?;
        let raw_uri = parts
            .next()
            .ok_or_else(|| ParseError::Invalid("Missing URI".to_string()))?;
        let version = parts
            .next()
            .ok_or_else(|| ParseError::Invalid("Missing HTTP version".to_string()))?
            .to_string();

        // Split URI and query string
        let (uri, query_string) = if let Some(pos) = raw_uri.find('?') {
            (raw_uri[..pos].to_string(), raw_uri[pos + 1..].to_string())
        } else {
            (raw_uri.to_string(), String::new())
        };

        // Parse headers
        let mut headers = HashMap::new();
        for line in header_lines {
            let line = strip_cr(line);
            if line.is_empty() {
                break;
            }
            if let Some(colon) = line.iter().position(|&b| b == b':') {
                let key = std::str::from_utf8(&line[..colon])
                    .map_err(|_| ParseError::Invalid("Non-UTF8 header key".to_string()))?
                    .trim()
                    .to_lowercase();
                let value = std::str::from_utf8(&line[colon + 1..])
                    .map_err(|_| ParseError::Invalid("Non-UTF8 header value".to_string()))?
                    .trim()
                    .to_string();
                headers.insert(key, value);
            }
        }

        let body_start = header_end + 4; // skip \r\n\r\n

        // Determine body length
        let body = if let Some(te) = headers.get("transfer-encoding") {
            if te.to_lowercase().contains("chunked") {
                let remaining = &self.buffer[body_start..];
                match decode_chunked(remaining) {
                    Some(decoded) => {
                        if decoded.len() > self.max_body_size {
                            return Err(ParseError::TooLarge);
                        }
                        decoded
                    }
                    None => return Ok(None), // not all chunks received yet
                }
            } else {
                Vec::new()
            }
        } else if let Some(cl) = headers.get("content-length") {
            let content_length: usize = cl
                .parse()
                .map_err(|_| ParseError::Invalid("Invalid Content-Length".to_string()))?;
            if content_length > self.max_body_size {
                return Err(ParseError::TooLarge);
            }
            let available = self.buffer.len() - body_start;
            if available < content_length {
                return Ok(None); // body not fully received yet
            }
            self.buffer[body_start..body_start + content_length].to_vec()
        } else {
            Vec::new()
        };

        Ok(Some(Request {
            method: Method::from_str(method_str),
            uri,
            version,
            headers,
            body,
            query_string,
        }))
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn strip_cr(line: &[u8]) -> &[u8] {
    if line.last() == Some(&b'\r') {
        &line[..line.len() - 1]
    } else {
        line
    }
}

fn decode_chunked(data: &[u8]) -> Option<Vec<u8>> {
    let mut result = Vec::new();
    let mut pos = 0;

    loop {
        // Find end of chunk size line
        let line_end = find_subsequence(&data[pos..], b"\r\n")?;
        let size_line = std::str::from_utf8(&data[pos..pos + line_end]).ok()?;
        let chunk_size = usize::from_str_radix(size_line.trim(), 16).ok()?;

        pos += line_end + 2;

        if chunk_size == 0 {
            return Some(result);
        }

        if pos + chunk_size + 2 > data.len() {
            return None; // incomplete
        }

        result.extend_from_slice(&data[pos..pos + chunk_size]);
        pos += chunk_size + 2; // skip trailing \r\n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_get() {
        let raw = b"GET /index.html HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut parser = RequestParser::new(1024 * 1024);
        parser.feed(raw);
        let req = parser.try_parse().unwrap().unwrap();
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.uri, "/index.html");
        assert_eq!(req.headers.get("host").map(|s| s.as_str()), Some("localhost"));
    }

    #[test]
    fn test_parse_post_with_body() {
        let body = b"name=foo&value=bar";
        let raw = format!(
            "POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut data = raw.into_bytes();
        data.extend_from_slice(body);
        let mut parser = RequestParser::new(1024 * 1024);
        parser.feed(&data);
        let req = parser.try_parse().unwrap().unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.body, body);
    }

    #[test]
    fn test_query_string() {
        let raw = b"GET /search?q=hello&page=2 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut parser = RequestParser::new(1024 * 1024);
        parser.feed(raw);
        let req = parser.try_parse().unwrap().unwrap();
        assert_eq!(req.uri, "/search");
        assert_eq!(req.query_string, "q=hello&page=2");
    }
}
