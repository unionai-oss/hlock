"""A fake decision-model (and chat) server for the tests."""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Fake:
    """A model server: System One at /v1/systemone, chat completions at
    /v1/chat/completions, and Ollama's /api/tags. `answer(path, body)` replies."""

    def __init__(self, answer):
        self.requests = []
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def reply(self, body):
                data = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                self.reply(answer(self.path, None))

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["content-length"])) or b"null")
                fake.requests.append((self.path, body))
                self.reply(answer(self.path, body))

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()


def systemone(body, noul=0.9, choice=None, p=0.95):
    answers = {}
    for qid, q in body["questions"].items():
        if q["type"] == "noul":
            answers[qid] = {"type": "noul", "noul": noul(qid, q, body) if callable(noul) else noul}
        elif q["type"] == "choice":
            c = choice(qid, q, body) if callable(choice) else (choice or next(iter(q["criteria"])))
            probs = {k: (p if k == c else (1 - p) / (len(q["criteria"]) - 1)) for k in q["criteria"]}
            answers[qid] = {"type": "choice", "choice": c, "probabilities": probs, "confidence": p}
        else:
            n = len(q["criteria"])
            answers[qid] = {"type": "score", "score": n - 1, "probabilities": {str(i): float(i == n - 1) for i in range(n)},
                            "confidence": 0.9}
    return {"model": f"{body['model']}-q8", "answers": answers, "usage": {"input_tokens": 42}}
