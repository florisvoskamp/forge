#!/bin/bash
cd "$(dirname "$0")"
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientName":"forge","clientVersion":"0.4.63"}}'
  sleep 1
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"thread/new","params":{}}'
  sleep 1
  printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"this/does/not/exist","params":{}}'
  sleep 2
} | timeout 6 codex exec-server --listen stdio 2>/dev/null
