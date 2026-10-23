#!/usr/bin/env python3
"""Minimal W3C WebDriver server for tests.

Usage: fake_webdriver.py [webdriver] [--account NAME] --port N
With --port 0 it binds a free port. Once listening it prints one JSON line
{"account", "port", "url"} (the `webkit-cli webdriver` readiness contract).
Each start appends "launch <account>" to $FAKE_WD_LOG when set.
"""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

args = sys.argv[1:]
if args[:1] == ["webdriver"]:
    args = args[1:]


def flag(name, default=None):
    return args[args.index(name) + 1] if name in args else default


port = int(flag("--port", "0"))
account = flag("--account")
state = {"url": "about:blank", "sessions": 0}

if os.environ.get("FAKE_WD_LOG"):
    with open(os.environ["FAKE_WD_LOG"], "a") as log:
        log.write("launch %s\n" % account)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, value, status=200):
        body = json.dumps({"value": value}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def do_POST(self):
        data = self.body()
        if self.path == "/session":
            state["sessions"] += 1
            sid = "s%d" % state["sessions"]
            state["sid"] = sid
            return self.reply({"sessionId": sid, "capabilities": {"account": account}})
        sid = state.get("sid")
        if self.path == "/session/%s/url" % sid:
            state["url"] = data["url"]
            return self.reply(None)
        return self.reply({"error": "unknown command", "message": self.path}, 404)

    def do_GET(self):
        sid = state.get("sid")
        if self.path == "/status":
            return self.reply({"ready": True, "message": "fake"})
        if self.path == "/session/%s/url" % sid:
            return self.reply(state["url"])
        if self.path == "/session/%s/title" % sid:
            return self.reply("Fake %s" % account)
        return self.reply({"error": "unknown command", "message": self.path}, 404)

    def do_DELETE(self):
        state.pop("sid", None)
        self.reply(None)


server = HTTPServer(("127.0.0.1", port), Handler)
real_port = server.server_address[1]
print(json.dumps({"account": account, "port": real_port, "url": "http://127.0.0.1:%d" % real_port}), flush=True)
server.serve_forever()
