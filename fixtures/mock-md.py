#!/usr/bin/env python3
"""Mock agent that replies with a markdown TABLE (+ blockquote, list) to exercise
awm's full-markdown rendering."""
import sys, json
def emit(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
emit({"type":"system","subtype":"init","permissionMode":"default","model":"mock-md","session_id":"md-1"})
table = ("## Project phases\n\n"
         "| Phase | Status | Notes |\n"
         "|-------|:------:|-------|\n"
         "| 0-2   | done   | scaffold, contracts, tracks |\n"
         "| 3     | done   | integration + runtime |\n"
         "| 4     | wip    | **hardening** & `--md` |\n\n"
         "> rendered as a real table now\n")
emit({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":table}],"usage":{"input_tokens":50,"output_tokens":40}}})
emit({"type":"result","subtype":"success","is_error":False,"result":"done","usage":{"input_tokens":50,"output_tokens":40}})
