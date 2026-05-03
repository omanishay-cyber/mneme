@echo off
REM Wrapper: launch the OFFICIAL graphifyy MCP server (graphify.serve)
REM
REM Original wrapper used `mcp_server.server` from the
REM `mcp-graphify-autotrigger 0.3.0` fork. That fork imports an `autotrigger`
REM package that is not actually shipped with it, so the MCP process started
REM cleanly, advertised tools, but every `tools/call` hung past the budget.
REM
REM `graphifyy 0.6.7` (the upstream pip package from
REM github.com/safishamsi/graphify) ships its own stdio MCP server in
REM `graphify/serve.py`. It uses the standard `mcp` Python package and
REM responds to `tools/call` in <50 ms against an existing graph.json,
REM so it gives Claude actual measured numbers in the bench instead of
REM a 0/10 timeout column.
REM
REM Probed 2026-05-02: 7 tools listed, query_graph + god_nodes returned
REM in ~10 ms each against the 3,929-node mneme corpus graph.
cd /d "D:\Mneme Dome\Mneme-Home-Handoff-2026-04-30-2027\source"
"C:\Python314\python.exe" -m graphify.serve "D:\Mneme Dome\Mneme-Home-Handoff-2026-04-30-2027\source\graphify-out\graph.json"
