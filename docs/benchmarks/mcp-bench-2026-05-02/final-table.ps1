param(
    [string]$ResultsDir = 'C:\Users\team\Desktop\temp\mcp-bench-2026-05-02\results-from-vm'
)

$mcps = @('mneme','tree-sitter','code-review-graph','graphify')
$queries = @('Q1','Q2','Q3','Q4','Q5')

# Build cells: { mcp -> query -> { wall_s, output_tokens, cost, score, note } }
$cells = @{}
$totals = @{}
foreach ($mcp in $mcps) {
    $cells[$mcp] = @{}
    $totals[$mcp] = @{ wall = 0; out = 0; cost = 0; score_sum = 0; n = 0 }
}

foreach ($mcp in $mcps) {
    foreach ($q in $queries) {
        $f = Join-Path $ResultsDir "$mcp-$q.json"
        $cell = @{ wall = -1; out = -1; cost = -1; score = -1; note = '' }
        if (-not (Test-Path $f)) {
            $cell.note = 'no_file'
            $cells[$mcp][$q] = $cell
            continue
        }
        $raw = Get-Content $f -Raw
        if ($raw -match '"empty":true') {
            $cell.note = 'empty (process killed by 480s timeout)'; $cell.score = 0
            $cells[$mcp][$q] = $cell; continue
        }
        if ($raw -match '"timeout":true') {
            $cell.note = 'timeout'; $cell.score = 0
            $cells[$mcp][$q] = $cell; continue
        }
        try {
            $j = $raw | ConvertFrom-Json
            $cell.wall = [int]($j.duration_ms / 1000)
            $cell.out = $j.usage.output_tokens
            $cell.cost = [math]::Round($j.total_cost_usd, 4)
            $resultText = $j.result -as [string]
            # Auto-score by markers
            if ($resultText -match '(?i)(cannot answer|not.+been built|no data|shard not found|empty results|no graph)') {
                $cell.score = 0
                $cell.note = 'no answer'
            } else {
                $hits = ([regex]::Matches($resultText, '(?i)(src/utils/auth\.ts|useAuthStore|LoginPage|StartScreen|ProtectedRoute|orgManager|techKeyManager|electron/main|hashPassword|verifyPassword|generateRecoveryCode|validatePasswordStrength|usePermission|machineId|keyGenerator|safeStorage|scrypt)') | Measure-Object).Count
                if ($hits -ge 12) { $cell.score = 9 }
                elseif ($hits -ge 8) { $cell.score = 8 }
                elseif ($hits -ge 5) { $cell.score = 7 }
                elseif ($hits -ge 3) { $cell.score = 5 }
                elseif ($hits -ge 1) { $cell.score = 4 }
                else { $cell.score = 2 }
                $cell.note = "$hits ground-truth markers"
            }
            $totals[$mcp].wall += $cell.wall
            $totals[$mcp].out += $cell.out
            $totals[$mcp].cost += $cell.cost
            $totals[$mcp].score_sum += $cell.score
            $totals[$mcp].n += 1
        } catch {
            $cell.note = "parse_error"; $cell.score = 0
        }
        $cells[$mcp][$q] = $cell
    }
}

# Output markdown table
function CellStr($c) {
    if ($c.wall -lt 0) { return $c.note }
    return "${($c.wall)}s / $($c.out)t / `$$($c.cost) / **$($c.score)**/10"
}

Write-Output ""
Write-Output "| Query | mneme | tree-sitter | CRG | graphify |"
Write-Output "|---|---|---|---|---|"
foreach ($q in $queries) {
    $row = "| $q | "
    foreach ($mcp in $mcps) {
        $c = $cells[$mcp][$q]
        if ($c.wall -lt 0) {
            $row += "$($c.note) | "
        } else {
            $row += "$($c.wall)s / $($c.out)t / `$$($c.cost) / **$($c.score)**/10 | "
        }
    }
    Write-Output $row.TrimEnd(' ').TrimEnd('|') + ' |'
}
Write-Output "| **Totals** | $($totals['mneme'].wall)s / $($totals['mneme'].out)t / `$$([math]::Round($totals['mneme'].cost,4)) / **$([int]($totals['mneme'].score_sum/[math]::Max(1,$totals['mneme'].n)))**/10 avg | $($totals['tree-sitter'].wall)s / $($totals['tree-sitter'].out)t / `$$([math]::Round($totals['tree-sitter'].cost,4)) / **$([int]($totals['tree-sitter'].score_sum/[math]::Max(1,$totals['tree-sitter'].n)))**/10 avg | $($totals['code-review-graph'].wall)s / $($totals['code-review-graph'].out)t / `$$([math]::Round($totals['code-review-graph'].cost,4)) / **$([int]($totals['code-review-graph'].score_sum/[math]::Max(1,$totals['code-review-graph'].n)))**/10 avg | $($totals['graphify'].wall)s / $($totals['graphify'].out)t / `$$([math]::Round($totals['graphify'].cost,4)) / **$([int]($totals['graphify'].score_sum/[math]::Max(1,$totals['graphify'].n)))**/10 avg |"

Write-Output ""
Write-Output "## Per-cell notes"
foreach ($mcp in $mcps) {
    foreach ($q in $queries) {
        $c = $cells[$mcp][$q]
        Write-Output "- $mcp/$q : score=$($c.score) note=`"$($c.note)`""
    }
}
