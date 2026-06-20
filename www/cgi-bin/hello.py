#!/usr/bin/env python3
import os
import sys

method = os.environ.get("REQUEST_METHOD", "GET")
content_length = int(os.environ.get("CONTENT_LENGTH", "0") or "0")
body = sys.stdin.buffer.read(content_length) if content_length > 0 else b""

html = f"""<!DOCTYPE html>
<html><head><title>CGI test</title></head>
<body>
<h1>Hello from CGI</h1>
<p>Method: {method}</p>
<p>Query string: {os.environ.get('QUERY_STRING', '')}</p>
<p>Body received: {len(body)} bytes</p>
</body></html>"""

out = html.encode("utf-8")
sys.stdout.buffer.write(b"Content-Type: text/html\r\n")
sys.stdout.buffer.write(f"Content-Length: {len(out)}\r\n\r\n".encode("utf-8"))
sys.stdout.buffer.write(out)
sys.stdout.buffer.flush()
