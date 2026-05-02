@echo off
cd /d "<project root>"
python -m mcp_server.server %*
