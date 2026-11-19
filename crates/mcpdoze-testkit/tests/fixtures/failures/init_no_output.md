# MCPDoze debug export

Synthetic fixture (placeholders only), shaped like `mcpdoze debug export` output.

## Replay

```json
{
  "how": "Synthetic fixture.",
  "scenarios": [
    {
      "expected_kind": "init_no_output",
      "server": "example",
      "transport": "stdio",
      "request": {
        "method": "tools/call",
        "bytes": 80,
        "shape": {"k#11223344": {"t": "string", "len": 12, "cls": "ascii"}, "k#55667788": {"t": "int", "digits": 3}}
      },
      "script": [{"on": {"method": "initialize"}, "do": []}],
      "note": "fixture"
    }
  ]
}
```
