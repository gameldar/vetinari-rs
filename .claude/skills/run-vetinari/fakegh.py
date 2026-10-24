"""A stand-in for GitHub's GraphQL search, so the queue has PRs to show.

Serves the list in the JSON file given as argv[1], re-read on every request,
so editing it changes what vetinari's next refresh sees.
Each entry: {"n": 101, "title": "...", "updated": "<RFC 3339>",
             "checks": "SUCCESS|FAILURE|PENDING", "decision": null|"APPROVED"|"CHANGES_REQUESTED",
             "draft": false}
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

STATE = sys.argv[1]
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8765


def pr(n, title, updated, checks="SUCCESS", decision=None, draft=False):
    return {
        "__typename": "PullRequest", "number": n, "title": title,
        "url": f"https://github.test/acme/app/pull/{n}", "isDraft": draft,
        "updatedAt": updated, "headRefName": f"feature-{n}", "baseRefName": "main",
        "reviewDecision": decision, "author": {"login": "teammate"},
        "repository": {"nameWithOwner": "acme/app", "url": "https://github.test/acme/app"},
        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": checks}}}]},
    }


class Handler(BaseHTTPRequestHandler):
    def reply(self, body):
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        with open(STATE) as f:
            nodes = [pr(**entry) for entry in json.load(f)]
        self.reply({"data": {"search": {"nodes": nodes}}})

    def do_GET(self):
        self.reply({"login": "drive"})

    def log_message(self, *args):
        pass


HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
