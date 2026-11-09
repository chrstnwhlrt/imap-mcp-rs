#!/usr/bin/env python3
"""Pass-through proxy in front of Ollama that keeps every chat request
with what the model said to it: the request body, the text, the
reasoning and the tool calls, one JSON line each. why.py reads them.

    proxy.py PORT UPSTREAM RECORD.jsonl [TURNS]

A client that hangs up before the answer has started is passed on to
Ollama at once, as Ollama would see it without the proxy: the assistant
drops questions to its gate it no longer needs. With TURNS, the number
of requests asked so far is kept there, counted as each reaches the
model, and behind it the number of conversations, for textproxy.py to
file every tool call under the request it came in; TURNS.log lists each
request with its number, for run.py to find the calls of an answer. A
request the assistant asks once more because it did not take the answer
counts neither as a new request nor as a new conversation.
"""
import datetime
import http.client
import http.server
import json
import os
import select
import socket
import sys
import threading
from urllib.parse import urlsplit
from zoneinfo import ZoneInfo

PORT, UPSTREAM, RECORD = int(sys.argv[1]), urlsplit(sys.argv[2]), sys.argv[3]
TURNS = sys.argv[4] if len(sys.argv) > 4 else None
LOCK = threading.Lock()
# The time zone of the world and of the assistant's clock line
ZONE = ZoneInfo("Europe/Berlin")
asked = conversations = 0
began = ""
# The messages of the last request that began an answer, how its answer went for the assistant (outcome) and which of
# its two repetitions the assistant has used on these messages
last = {"messages": None, "outcome": "taken", "unthought": False, "retried": False}
# What the assistant takes for a passing fault of the provider (LlmFailure::classify, Transport)
TRANSPORT = ("connection", "connect error", "error sending request", "reset by peer", "broken pipe", "dns", "503", "502")


def first_of_an_answer(raw):
    """Whether a chat request is the first model call of an answer (with the
    tools, and the request itself as its last message), and the request
    where it begins a conversation (the request is its only one)."""
    try:
        body = json.loads(raw)
    except ValueError:
        return False, None
    messages = body.get("messages") or []
    first = bool(body.get("tools")) and bool(messages) and messages[-1].get("role") == "user"
    begins = first and sum(m.get("role") == "user" for m in messages) == 1
    return first, (messages[-1].get("content") or "") if begins else None


def outcome(body, answer, status, error, client_left):
    """How the assistant takes the answer to a request that begins one, as Otomo's src/llm/chat.rs does (LlmFailure):
    "taken" with a tool call or a word; "transient" for no word at all (Empty) or a fault of the connection
    (Transport), "timeout" when it hung up waiting for a first word while the model was to think; "final" for any other
    error, an answer cut off by its length and a timeout without thinking, after which it does not ask again. An
    answer that begins without the thought asked for ("unthought") the stream marks itself, before the client sees it."""
    if status >= 400:
        return "transient" if any(word in error.lower() for word in TRANSPORT) or status in (502, 503) else "final"
    if answer["calls"] or answer["content"] != "":
        return "taken"
    if client_left:
        return "timeout" if body.get("think") else "final"
    return "final" if answer.get("done_reason") == "length" else "transient"


def asked_again(messages):
    """Whether a request that begins an answer is the assistant asking the last one once more: after an unthought
    answer once without thinking, after a transient fault or a timeout while thinking once more, then it gives up.
    Called under LOCK."""
    if messages != last["messages"]:
        return False
    if last["outcome"] == "unthought" and not last["unthought"]:
        last["unthought"] = True
        return True
    if last["outcome"] in ("transient", "timeout") and not last["retried"]:
        last["retried"] = True
        return True
    return False


def unthought(body, lines, state):
    """Whether the lines so far begin an answer with a word, without the thought the request asked for and without a
    tool call (Otomo's chat.rs: the first words of an answer without a thought end it as Unthought). `state` keeps
    what came."""
    for line in lines:
        try:
            message = json.loads(line).get("message") or {}
        except ValueError:
            continue
        state["thought"] |= bool(message.get("thinking"))
        state["decided"] |= bool(message.get("tool_calls"))
        if not state["worded"] and (message.get("content") or "").strip():
            state["worded"] = True
            if body.get("think") and not state["thought"] and not state["decided"]:
                return True
    return False


def ended(lines):
    """Whether one of the lines of a streamed answer is its last one (done)."""
    for line in lines:
        try:
            if json.loads(line).get("done"):
                return True
        except (ValueError, AttributeError):
            continue
    return False


def last_request(raw):
    """The text of the request a chat call answers."""
    try:
        messages = json.loads(raw).get("messages") or []
    except ValueError:
        return ""
    return (messages[-1].get("content") or "") if messages else ""


def said(lines):
    """The text, the reasoning and the tool calls over the lines of one
    answer, with the tokens of the prompt and of the answer."""
    content, thinking, calls, counts = "", "", [], {}
    for line in lines:
        try:
            chunk = json.loads(line)
        except ValueError:
            continue
        message = chunk.get("message") or {}
        content += message.get("content") or ""
        thinking += message.get("thinking") or ""
        calls += message.get("tool_calls") or []
        counts.update({key: chunk[key] for key in ("prompt_eval_count", "eval_count", "done_reason") if key in chunk})
    return {"content": content, "thinking": thinking, "calls": calls, **counts}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.forward("GET")

    def do_POST(self):
        self.forward("POST")

    def do_DELETE(self):
        self.forward("DELETE")

    def forward(self, method):
        global asked, conversations, began
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        first, begins = first_of_an_answer(raw) if TURNS and self.path.startswith("/api/chat") else (False, None)
        body = json.loads(raw) if first else {}
        messages = body.get("messages")
        if first:
            # Before the model answers, and so before any tool it calls,
            # for textproxy.py to file the calls under the new count. A
            # request asked once more after an answer the assistant did not
            # take is still the one before.
            with LOCK:
                again = asked_again(messages)
                if not again:
                    last.update(unthought=False, retried=False)
                # Until its answer says otherwise: the same messages before it can only mean the assistant hung up waiting
                last.update(messages=messages, outcome="timeout" if body.get("think") else "final")
                if not again:
                    asked += 1
                    if begins is not None:
                        conversations += 1
                        began = datetime.datetime.now(ZONE).replace(second=0, microsecond=0).isoformat()
                    with open(TURNS + ".new", "w") as f:
                        f.write(f"{asked} {conversations}")
                    os.replace(TURNS + ".new", TURNS)
                    # Which request each number stands for, for run.py to find
                    # the calls of an answer in textproxy.py's log.
                    with open(TURNS + ".log", "a") as f:
                        f.write(json.dumps({"asked": asked, "text": last_request(raw), "now": began}, ensure_ascii=False) + "\n")

        def settle(result):
            """How the assistant took the answer to this request, kept before it sees the end of the answer, for the
            next request to tell whether it asks again."""
            with LOCK:
                if first and last["messages"] is messages and last["outcome"] != "unthought":
                    last["outcome"] = result

        # Two minutes without a byte from Ollama is a request stuck in its
        # runner (the drafter's hang, 15.10.2026): the client gets the error
        # and the run goes on with its next request.
        upstream = http.client.HTTPConnection(UPSTREAM.hostname, UPSTREAM.port or 80, timeout=120)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "content-length", "connection")}
        headers["Content-Length"] = str(len(raw))
        started, left = threading.Event(), threading.Event()
        try:
            upstream.request(method, self.path, body=raw, headers=headers)
            threading.Thread(target=self.watch, args=(upstream, started, left), daemon=True).start()
            resp = upstream.getresponse()
        except (OSError, http.client.HTTPException):
            started.set()
            # The assistant hung up waiting (the outcome stays as it is), or it finds the connection closed
            if not left.is_set():
                settle("transient")
            self.close_connection = True
            return
        started.set()
        self.send_response(resp.status)
        for k, v in resp.getheaders():
            if k.lower() not in ("transfer-encoding", "content-length", "connection"):
                self.send_header(k, v)
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        lines, rest, gone, broken = [], b"", False, False
        state = {"thought": False, "decided": False, "worded": False}
        while True:
            try:
                # An error comes whole, its words decide whether the assistant asks again
                chunk = resp.read() if resp.status >= 400 else resp.read1(65536)
            except (OSError, http.client.HTTPException):
                gone = broken = True
                break
            if not chunk:
                break
            *complete, rest = (rest + chunk).split(b"\n")
            if resp.status >= 400:
                complete, rest = [*complete, rest], b""
            complete = [line for line in complete if line.strip()]
            lines += complete
            if first and resp.status >= 400:
                settle(outcome(body, said([]), resp.status, b"\n".join(lines).decode("utf-8", "replace"), False))
            elif first:
                # Before the client sees the first words: it hangs up on them and asks again at once
                if unthought(body, complete, state):
                    with LOCK:
                        if last["messages"] is messages and not last["unthought"]:
                            last["outcome"] = "unthought"
                # Before the client sees the end of the answer
                if ended(complete):
                    settle(outcome(body, said(lines), resp.status, "", False))
            try:
                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                gone = True
                if first:
                    settle(outcome(body, said(lines), resp.status, "", True))
                break
        if rest.strip():
            lines.append(rest)
        # A stream Ollama cut off looks to the assistant like a broken connection
        if first and broken:
            settle("transient" if not said(lines)["content"] and not said(lines)["calls"] else "taken")
        if not gone:
            try:
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                gone = True
        upstream.close()
        self.close_connection = gone
        if self.path.startswith("/api/chat") and raw:
            try:
                recorded = json.loads(raw)
            except ValueError:
                return
            with LOCK, open(RECORD, "a") as f:
                f.write(json.dumps({"body": recorded, **said(lines)}, ensure_ascii=False) + "\n")

    def watch(self, upstream, started, left):
        """Closes the request to Ollama when the client hangs up before the
        answer has started, and says so in `left`."""
        while not started.is_set():
            try:
                ready, _, _ = select.select([self.connection], [], [], 0.02)
                if not ready:
                    continue
                # A client that has read its answer and gone is no reason to close anything any more
                if not self.connection.recv(1, socket.MSG_PEEK) and not started.is_set():
                    left.set()
                    upstream.sock.shutdown(socket.SHUT_RDWR)
                return
            except (OSError, AttributeError):
                return


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
