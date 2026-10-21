"""Isolated release-binary lifecycle test; uses a local HTTP stub, never a user's DB.

Run: python scripts/verify_daemon.py [path/to/remainder.exe]
"""
import http.server
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/remainder.exe").resolve()
state = {"active": 0, "peak": 0, "fail": False, "requests": 0}
lock = threading.Lock()


class Stub(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with lock:
            state["active"] += 1
            state["requests"] += 1
            state["peak"] = max(state["peak"], state["active"])
            fail = state["fail"]
        try:
            time.sleep(0.35)
            prompt = payload["prompt"]
            if "\nInput: " in prompt and "\n\n### Response:" in prompt:
                text = prompt.split("\nInput: ", 1)[1].split("\n\n### Response:", 1)[0]
            elif "Input:\n" in prompt and "\n\nPropositions:\n" in prompt:
                text = prompt.split("Input:\n", 1)[1].split("\n\nPropositions:\n", 1)[0]
            else:
                text = prompt
            body = json.dumps({"response": "[TEST] -> " + text.strip()}).encode()
            self.send_response(503 if fail else 200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        finally:
            with lock:
                state["active"] -= 1


def wait_for(check, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if check():
                print("PASS:", description, flush=True)
                return
        except (sqlite3.OperationalError, FileNotFoundError):
            pass
        time.sleep(0.1)
    raise AssertionError(description)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Stub)
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with tempfile.TemporaryDirectory(prefix="remainder-daemon-test-") as directory:
        root = Path(directory)
        database = root / "isolated.db"
        for i in range(6):
            (root / f"file{i}.PY").write_text(f"INITIALTOKEN{i} enabled.", encoding="utf-8")
        env = dict(os.environ, REMAINDER_DB=str(database),
                   REMAINDER_OLLAMA_URL=f"http://127.0.0.1:{server.server_port}")
        options = dict(cwd=root, env=env, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))

        def query(sql, parameters=()):
            with closing(sqlite3.connect(database, timeout=2)) as connection:
                return connection.execute(sql, parameters).fetchall()

        with (root / "daemon.log").open("w", encoding="utf-8") as log:
            proc = subprocess.Popen([binary], stdout=log, stderr=log, **options)
            try:
                wait_for(lambda: query("SELECT count(*) FROM sources WHERE facts_current=1")[0][0] == 6,
                         "startup ingests six pre-existing uppercase-extension Python files")
                assert state["peak"] == 2, state
                print("PASS: observed HTTP concurrency peak = 2", flush=True)
                messages = [
                    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
                    {"jsonrpc": "2.0", "method": "notifications/initialized"},
                    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
                    {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                        "name": "query_bunker", "arguments": {"query": "INITIALTOKEN0", "limit": 1}}},
                ]
                reply = subprocess.run([binary, "mcp"], input="".join(json.dumps(m) + "\n" for m in messages),
                                       text=True, capture_output=True, timeout=10, check=True, **options)
                replies = [json.loads(line) for line in reply.stdout.splitlines()]
                assert [r["id"] for r in replies] == [1, 2, 3]
                assert "INITIALTOKEN0" in replies[2]["result"]["content"][0]["text"]
                print("PASS: release executable stdio MCP round trip", flush=True)
                with lock:
                    state["fail"] = True
                (root / "file0.PY").write_text("REPLACEMENTTOKEN enabled.", encoding="utf-8")
                wait_for(lambda: query("SELECT facts_current,raw_content FROM sources WHERE file_path LIKE '%file0.PY'")
                         == [(0, "REPLACEMENTTOKEN enabled.")], "changed source is stored while extraction fails")
                old = subprocess.run([binary, "query", "INITIALTOKEN0"], capture_output=True, text=True,
                                     timeout=10, check=True, **options)
                assert "No records found" in old.stdout
                new = subprocess.run([binary, "query", "REPLACEMENTTOKEN"], capture_output=True, text=True,
                                     timeout=10, check=True, **options)
                assert "[SOURCE]" in new.stdout
                print("PASS: stale fact hidden; new source searchable during HTTP failure", flush=True)
                with lock:
                    state["fail"] = False
                wait_for(lambda: query("SELECT facts_current FROM sources WHERE file_path LIKE '%file0.PY'") == [(1,)],
                         "automatic retry commits new facts without another edit")
                (root / "file1.PY").unlink()
                wait_for(lambda: query("SELECT count(*) FROM sources")[0][0] == 5, "watcher propagates deletion")
                proc.terminate()
                proc.wait(timeout=5)
                (root / "file0.PY").write_text("OFFLINEUPDATE enabled.", encoding="utf-8")
                (root / "new.ts").write_text("OFFLINENEW enabled.", encoding="utf-8")
                proc = subprocess.Popen([binary], stdout=log, stderr=log, **options)
                wait_for(lambda: query("SELECT count(*) FROM sources WHERE facts_current=1")[0][0] == 6
                         and query("SELECT count(*) FROM atomics WHERE fact_content LIKE '%OFFLINEUPDATE%'")[0][0] == 1,
                         "restart discovers offline edits and new TypeScript files")
                assert query("PRAGMA integrity_check") == [("ok",)]
                assert query("PRAGMA foreign_key_check") == []
                print("PASS: post-lifecycle database integrity", flush=True)
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    proc.wait(timeout=5)
finally:
    server.shutdown()
    server.server_close()
