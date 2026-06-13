use crate::config::{RouteConfig, ServerConfig};
use crate::request::Request;

pub struct RouteMatch<'a> {
    pub route: &'a RouteConfig,
    pub server: &'a ServerConfig,
}

pub fn match_server<'a>(
    servers: &'a [ServerConfig],
    host_header: &str,
    port: u16,
) -> &'a ServerConfig {
    let host_name = host_header.split(':').next().unwrap_or(host_header);

    // First: match by server_name + port
    for server in servers {
        if server.ports.contains(&port) {
            for name in &server.server_names {
                if name == host_name {
                    return server;
                }
            }
        }
    }

    // Fallback: first server that listens on this port
    for server in servers {
        if server.ports.contains(&port) {
            return server;
        }
    }

    &servers[0]
}

pub fn match_route<'a>(server: &'a ServerConfig, req: &Request) -> Option<RouteMatch<'a>> {
    let uri = &req.uri;

    // Find the longest matching prefix
    let mut best: Option<&RouteConfig> = None;
    let mut best_len = 0;

    for route in &server.routes {
        if uri.starts_with(&route.path) {
            let len = route.path.len();
            // Ensure we matched at a path boundary (avoid /foo matching /foobar)
            let boundary_ok = uri.len() == len
                || uri.as_bytes().get(len) == Some(&b'/')
                || route.path == "/";
            if boundary_ok && len >= best_len {
                best_len = len;
                best = Some(route);
            }
        }
    }

    best.map(|route| RouteMatch { route, server })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouteConfig, ServerConfig};
    use crate::request::{Method, Request};
    use std::collections::HashMap;

    fn make_server(port: u16, routes: Vec<RouteConfig>) -> ServerConfig {
        ServerConfig {
            host: "0.0.0.0".to_string(),
            ports: vec![port],
            server_names: vec!["localhost".to_string()],
            error_pages: HashMap::new(),
            client_max_body_size: 1024 * 1024,
            routes,
        }
    }

    fn make_route(path: &str) -> RouteConfig {
        RouteConfig {
            path: path.to_string(),
            allowed_methods: vec!["GET".to_string()],
            redirect: None,
            root: Some("/var/www".to_string()),
            default_file: None,
            cgi_extension: None,
            directory_listing: false,
            upload_dir: None,
        }
    }

    fn make_request(uri: &str) -> Request {
        Request {
            method: Method::Get,
            uri: uri.to_string(),
            version: "HTTP/1.1".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
            query_string: String::new(),
        }
    }

    #[test]
    fn test_longest_prefix_match() {
        let server = make_server(8080, vec![make_route("/"), make_route("/api")]);
        let req = make_request("/api/users");
        let m = match_route(&server, &req).unwrap();
        assert_eq!(m.route.path, "/api");
    }

    #[test]
    fn test_root_fallback() {
        let server = make_server(8080, vec![make_route("/"), make_route("/api")]);
        let req = make_request("/about");
        let m = match_route(&server, &req).unwrap();
        assert_eq!(m.route.path, "/");
    }
}
