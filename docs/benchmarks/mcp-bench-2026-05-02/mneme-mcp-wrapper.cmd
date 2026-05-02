@echo off
REM This wrapper ensures mneme MCP is launched from the project directory
cd /d "<project root>"
"<home>\.mneme\bin\mneme.exe" mcp stdio
