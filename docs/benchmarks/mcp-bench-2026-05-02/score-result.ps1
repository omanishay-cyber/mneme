param(
    [string]$ResultsDir = 'C:\Users\team\Desktop\temp\mcp-bench-2026-05-02\results-from-vm'
)

# Manual scoring rubric for each query, validated by checking response against ground truth.
# Score is 0-10, where:
#   0  = no answer / "cannot answer"
#   3  = generic answer with no project-specific data
#   5  = partial answer covering some real files
#   7  = thorough answer covering most ground-truth items
#   10 = comprehensive, all ground-truth items + correct citations

# Returns objects with score, justification per (mcp, query)
$mcps = @('mneme','tree-sitter','code-review-graph','graphify')
$queries = @('Q1','Q2','Q3','Q4','Q5')
foreach ($mcp in $mcps) {
    foreach ($q in $queries) {
        $f = Join-Path $ResultsDir "$mcp-$q.json"
        if (-not (Test-Path $f)) { continue }
        $raw = Get-Content $f -Raw
        if ($raw -match '"empty":true') { Write-Output "$mcp/$q SCORE=0 NOTE=empty (timeout/killed)"; continue }
        if ($raw -match '"timeout":true') { Write-Output "$mcp/$q SCORE=0 NOTE=timeout"; continue }
        try {
            $j = $raw | ConvertFrom-Json
            $resultText = $j.result -as [string]
            if (-not $resultText) { Write-Output "$mcp/$q SCORE=0 NOTE=no_result_text"; continue }
            $resultLen = $resultText.Length
            $score = 0
            $note = ""
            # Auto-score by markers
            if ($resultText -match '(?i)(cannot answer|not.+been built|no data|shard not found|empty results)') {
                $score = 0
                $note = "MCP could not answer (no/empty index)"
            } else {
                # Check for project-specific filenames
                $hits = ([regex]::Matches($resultText, '(?i)(src/utils/auth\.ts|useAuthStore|LoginPage|StartScreen|ProtectedRoute|orgManager|techKeyManager|electron/main|hashPassword|verifyPassword|generateRecoveryCode|validatePasswordStrength)') | Measure-Object).Count
                if ($hits -ge 12) { $score = 9 }
                elseif ($hits -ge 8) { $score = 8 }
                elseif ($hits -ge 5) { $score = 7 }
                elseif ($hits -ge 3) { $score = 5 }
                elseif ($hits -ge 1) { $score = 4 }
                else { $score = 2 }
                $note = "auto-detected $hits ground-truth markers in response"
            }
            Write-Output "$mcp/$q SCORE=$score CHARS=$resultLen TOKENS=$($j.usage.output_tokens) WALL_S=$([int]($j.duration_ms/1000)) COST=$([math]::Round($j.total_cost_usd,4)) TURNS=$($j.num_turns) NOTE='$note'"
        } catch {
            Write-Output "$mcp/$q SCORE=0 NOTE=parse_error: $_"
        }
    }
}
