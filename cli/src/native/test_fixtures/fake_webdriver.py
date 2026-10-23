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
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

args = sys.argv[1:]
if args[:1] == ["--version"]:
    print(os.environ.get("FAKE_WD_VERSION", "webkit-cli 0.5.0"))
    sys.exit(0)
if args[:1] == ["stop"]:
    # `webkit-cli stop --account NAME`
    if os.environ.get("FAKE_WD_LOG"):
        with open(os.environ["FAKE_WD_LOG"], "a") as log:
            log.write("stop %s\n" % args[args.index("--account") + 1])
    print(json.dumps({"stopped": True}))
    sys.exit(0)
if args[:1] == ["webdriver"]:
    args = args[1:]


def flag(name, default=None):
    return args[args.index(name) + 1] if name in args else default


port = int(flag("--port", "0"))
account = flag("--account")
state = {"url": "about:blank", "sessions": 0, "origin": 1.0}


def commit_pending_navigation():
    """A click on a "slowlink" page starts a navigation that commits 1s later."""
    if state.get("commit_at") and time.time() >= state["commit_at"]:
        state["url"] = state.pop("pending")
        state.pop("commit_at")
        state["origin"] += 1

if os.environ.get("FAKE_WD_LOG"):
    with open(os.environ["FAKE_WD_LOG"], "a") as log:
        log.write("launch %s\n" % account)


ELEMENT = {"element-6066-11e4-a52e-4f735466cecf": "el-7"}
SNAPSHOT = {
    "document": "fake-doc",
    "nodes": [
        {"nodeId": "0", "role": {"type": "role", "value": "RootWebArea"}, "childIds": ["1"]},
        {
            "nodeId": "1",
            "role": {"type": "role", "value": "button"},
            "name": {"type": "computedString", "value": "Save"},
            "backendDOMNodeId": 7,
            "childIds": [],
        },
    ],
    "cursor": [],
    "hrefs": {},
}


def execute(script, args=()):
    """Canned answers for agent-browser's DOM-walk scripts (no JS engine here)."""
    commit_pending_navigation()
    if "navigation.addEventListener('navigate'" in script:
        return {"pending": state.get("pending"), "ready": True, "url": state["url"], "origin": state["origin"]}
    if "__abOpts" in script:
        return ELEMENT if '"resolve"' in script else SNAPSHOT
    if "wanted = arguments[1]" in script:
        # select: report options that do not exist
        missing = [w for w in args[1] if w not in ("free", "pro")]
        return "no option %s" % json.dumps(missing) if missing else None
    if "querySelectorAll(arguments[0]).length" in script:
        return 3
    if "v == null ? '' : String(v)" in script:
        return state.get("typed", "")
    if "innerText" in script:
        if state["url"].endswith("/arrived"):
            return "Arrived page"
        return "Save (clicked %d)" % state.get("clicks", 0)
    return None


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
            caps = data.get("capabilities", {}).get("alwaysMatch", {})
            state["page_load_ms"] = caps.get("timeouts", {}).get("pageLoad")
            state["strategy"] = caps.get("pageLoadStrategy")
            sid = "s%d" % state["sessions"]
            state["sid"] = sid
            return self.reply({"sessionId": sid, "capabilities": {"account": account}})
        sid = state.get("sid")
        # Like webkit-cli, this server is single-threaded: while a load is
        # pending, no other request is answered. "hang.test" never finishes
        # loading, "slow.test" takes 2s, and on a "hang-later" page a click or
        # back starts a load that never finishes.
        if self.path == "/session/%s/url" % sid:
            if "deadline.test" in data["url"]:
                # webkit-cli 0.5.0: enforce the session pageLoad, answer
                # `timeout`, and keep serving later commands.
                time.sleep((state.get("page_load_ms") or 0) / 1000.0)
                state["url"] = data["url"]
                return self.reply({"error": "timeout", "message": "goto timed out"}, 500)
            if "hang.test" in data["url"]:
                time.sleep(3600)
            if "slow.test" in data["url"]:
                time.sleep(2)
            state["url"] = data["url"]
            state["origin"] += 1
            return self.reply(None)
        if self.path == "/session/%s/element" % sid:
            return self.reply(ELEMENT)
        if self.path == "/session/%s/timeouts" % sid:
            state["page_load_ms"] = data.get("pageLoad", state.get("page_load_ms"))
            return self.reply(None)
        if self.path == "/session/%s/execute/sync" % sid:
            return self.reply(execute(data["script"], data.get("args", [])))
        if self.path == "/session/%s/element/el-7/value" % sid:
            state["typed"] = state.get("typed", "") + data["text"]
            return self.reply(None)
        if self.path == "/session/%s/element/el-7/clear" % sid:
            state["typed"] = ""
            return self.reply(None)
        if self.path == "/session/%s/actions" % sid:
            state.setdefault("actions", []).append(data)
            return self.reply(None)
        if self.path in ("/session/%s/element/el-7/click" % sid, "/session/%s/back" % sid):
            if "hang-later" in state["url"]:
                time.sleep(3600)
        if self.path == "/session/%s/element/el-7/click" % sid:
            state["clicks"] = state.get("clicks", 0) + 1
            if "slowlink" in state["url"]:
                state["pending"] = state["url"].split("/slowlink")[0] + "/arrived"
                state["commit_at"] = time.time() + 1.0
            return self.reply(None)
        if self.path == "/session/%s/back" % sid:
            state["origin"] += 1
            return self.reply(None)
        return self.reply({"error": "unknown command", "message": self.path}, 404)

    def do_GET(self):
        sid = state.get("sid")
        if self.path == "/status":
            return self.reply({"ready": True, "message": "fake"})
        if self.path == "/session/%s/url" % sid:
            commit_pending_navigation()
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
