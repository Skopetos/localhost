use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub ports: Vec<u16>,
    pub server_names: Vec<String>,
    pub error_pages: HashMap<u16, String>,
    pub client_max_body_size: usize,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone)]
pub struct RouteConfig {
    pub path: String,
    pub allowed_methods: Vec<String>,
    pub redirect: Option<(u16, String)>,
    pub root: Option<String>,
    pub default_file: Option<String>,
    pub cgi_extension: Option<String>,
    pub directory_listing: bool,
    pub upload_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
}

pub fn parse_config(path: &str) -> Result<Config, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read config file '{}': {}", path, e))?;
    parse_config_str(&content)
}

fn parse_config_str(content: &str) -> Result<Config, String> {
    let mut servers = Vec::new();
    let mut lines = content.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed == "server {" || trimmed == "server{" {
            let server = parse_server_block(&mut lines)?;
            servers.push(server);
        }
    }

    if servers.is_empty() {
        return Err("No server blocks found in config".to_string());
    }

    validate_config(&servers)?;

    Ok(Config { servers })
}

/// Catches genuine port conflicts: the same host:port declared twice within
/// one server block, or two server blocks claiming the same host:port with
/// no way to distinguish between them (same server_name, or both wildcard/
/// catch-all with no server_name at all). Different server_names sharing a
/// host:port is normal virtual hosting and is left alone.
fn validate_config(servers: &[ServerConfig]) -> Result<(), String> {
    for server in servers {
        let mut seen = std::collections::HashSet::new();
        for &port in &server.ports {
            if !seen.insert(port) {
                return Err(format!(
                    "Duplicate 'listen {}' in the same server block ({})",
                    port, server.host
                ));
            }
        }
    }

    for i in 0..servers.len() {
        for j in (i + 1)..servers.len() {
            let a = &servers[i];
            let b = &servers[j];
            if a.host != b.host {
                continue;
            }
            for &port in &a.ports {
                if !b.ports.contains(&port) {
                    continue;
                }
                let conflict = if a.server_names.is_empty() && b.server_names.is_empty() {
                    true
                } else {
                    a.server_names.iter().any(|n| b.server_names.contains(n))
                };
                if conflict {
                    return Err(format!(
                        "Conflicting configuration: {}:{} is claimed by more than one server block \
                         with no distinguishing server_name",
                        a.host, port
                    ));
                }
            }
        }
    }

    Ok(())
}

fn parse_server_block<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Result<ServerConfig, String> {
    let mut host = "0.0.0.0".to_string();
    let mut ports = Vec::new();
    let mut server_names = Vec::new();
    let mut error_pages = HashMap::new();
    let mut client_max_body_size = 1024 * 1024; // 1MB default
    let mut routes = Vec::new();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed == "}" {
            break;
        }
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("location ") {
            let path = trimmed
                .trim_start_matches("location ")
                .trim_end_matches('{')
                .trim()
                .to_string();
            let route = parse_location_block(path, lines)?;
            routes.push(route);
        } else if let Some(value) = directive_value(trimmed, "host") {
            host = value.to_string();
        } else if let Some(value) = directive_value(trimmed, "listen") {
            let port: u16 = value
                .parse()
                .map_err(|_| format!("Invalid port: {}", value))?;
            ports.push(port);
        } else if let Some(value) = directive_value(trimmed, "server_name") {
            server_names.extend(value.split_whitespace().map(str::to_string));
        } else if let Some(value) = directive_value(trimmed, "client_max_body_size") {
            client_max_body_size = parse_size(value)?;
        } else if trimmed.starts_with("error_page ") {
            let parts: Vec<&str> = trimmed
                .trim_start_matches("error_page ")
                .trim_end_matches(';')
                .splitn(2, ' ')
                .collect();
            if parts.len() == 2 {
                let code: u16 = parts[0]
                    .parse()
                    .map_err(|_| format!("Invalid error code: {}", parts[0]))?;
                error_pages.insert(code, parts[1].to_string());
            }
        }
    }

    if ports.is_empty() {
        return Err("Server block missing 'listen' directive".to_string());
    }

    Ok(ServerConfig {
        host,
        ports,
        server_names,
        error_pages,
        client_max_body_size,
        routes,
    })
}

fn parse_location_block<'a>(
    path: String,
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Result<RouteConfig, String> {
    let mut allowed_methods = vec!["GET".to_string()];
    let mut redirect = None;
    let mut root = None;
    let mut default_file = None;
    let mut cgi_extension = None;
    let mut directory_listing = false;
    let mut upload_dir = None;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed == "}" {
            break;
        }
        if trimmed.is_empty() {
            continue;
        }

        if let Some(value) = directive_value(trimmed, "allow_methods") {
            allowed_methods = value.split_whitespace().map(str::to_string).collect();
        } else if trimmed.starts_with("return ") {
            let parts: Vec<&str> = trimmed
                .trim_start_matches("return ")
                .trim_end_matches(';')
                .splitn(2, ' ')
                .collect();
            if parts.len() == 2 {
                let code: u16 = parts[0]
                    .parse()
                    .map_err(|_| format!("Invalid redirect code: {}", parts[0]))?;
                redirect = Some((code, parts[1].to_string()));
            }
        } else if let Some(value) = directive_value(trimmed, "root") {
            root = Some(value.to_string());
        } else if let Some(value) = directive_value(trimmed, "index") {
            default_file = Some(value.to_string());
        } else if let Some(value) = directive_value(trimmed, "cgi_extension") {
            cgi_extension = Some(value.to_string());
        } else if let Some(value) = directive_value(trimmed, "autoindex") {
            directory_listing = value == "on";
        } else if let Some(value) = directive_value(trimmed, "upload_dir") {
            upload_dir = Some(value.to_string());
        }
    }

    Ok(RouteConfig {
        path,
        allowed_methods,
        redirect,
        root,
        default_file,
        cgi_extension,
        directory_listing,
        upload_dir,
    })
}

fn directive_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{} ", name);
    if line.starts_with(&prefix) {
        Some(line[prefix.len()..].trim_end_matches(';').trim())
    } else {
        None
    }
}

fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim_end_matches(';');
    if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        Ok(n.parse::<usize>().map_err(|_| format!("Invalid size: {}", s))? * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('K').or_else(|| s.strip_suffix('k')) {
        Ok(n.parse::<usize>().map_err(|_| format!("Invalid size: {}", s))? * 1024)
    } else {
        s.parse::<usize>().map_err(|_| format!("Invalid size: {}", s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_config() {
        let cfg = r#"
server {
    host 127.0.0.1;
    listen 8080;
    server_name localhost;
    client_max_body_size 10M;

    location / {
        root /var/www/html;
        index index.html;
        allow_methods GET POST;
        autoindex off;
    }
}
"#;
        let config = parse_config_str(cfg).unwrap();
        assert_eq!(config.servers.len(), 1);
        let s = &config.servers[0];
        assert_eq!(s.host, "127.0.0.1");
        assert_eq!(s.ports, vec![8080]);
        assert_eq!(s.client_max_body_size, 10 * 1024 * 1024);
        assert_eq!(s.routes.len(), 1);
        assert_eq!(s.routes[0].path, "/");
        assert_eq!(s.routes[0].root, Some("/var/www/html".to_string()));
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("10M").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("512K").unwrap(), 512 * 1024);
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }
}
