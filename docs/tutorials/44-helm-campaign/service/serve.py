import json
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        query = urllib.parse.urlparse(self.path).query
        params = urllib.parse.parse_qs(query)
        mode = int(params.get("mode", ["0"])[0])
        retry = int(params.get("retry", ["0"])[0])
        status = "corrupt" if mode == 1 and retry == 2 else "ok"
        body = json.dumps(
            {"mode": mode, "retry": retry, "status": status}
        ).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


HTTPServer(("127.0.0.1", 8080), Handler).serve_forever()
