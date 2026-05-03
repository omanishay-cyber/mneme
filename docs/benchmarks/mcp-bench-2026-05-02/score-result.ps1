param(
    [string]$ResultsDir = 'C:\Users\Anish\Desktop\temp\mcp-bench-2026-05-02\results'
)

$markersPattern = 'build_or_migrate|inject_file|Store::new|DbBuilder|PathManager|IncrementalParser|parse_file|index_files|build_pipeline|tree_to_nodes|paths::PathManager|use mneme_common::paths|mneme_home_override|MNEME_HOME|common/src/paths|Store::open|rusqlite|graph\.db|worker_ipc|livebus|parser_pool|migrate|r2d2|Mutex|Atomic|busy_timeout|RwLock|Singleton|Facade|Builder|build\.rs|builder\.rs|inject\.rs|paths\.rs|lib\.rs'

$mcps = @('mneme','tree-sitter','code-review-graph','graphify')
$queries = @('Q1','Q2','Q3','Q4','Q5')

foreach ($mcp in $mcps) {
    foreach ($q in $queries) {
        $f = Join-Path $ResultsDir "$mcp-$q.json"
        if (-not (Test-Path $f)) {
            Write-Output "$mcp/$q SCORE=0 NOTE=no_file"
            continue
        }
        $raw = Get-Content $f -Raw -ErrorAction SilentlyContinue
        if ([string]::IsNullOrWhiteSpace($raw)) {
            Write-Output "$mcp/$q SCORE=0 NOTE=empty_file"
            continue
        }
        try {
            $j = $raw | ConvertFrom-Json
            $resultText = $j.result -as [string]
            if (-not $resultText) {
                Write-Output "$mcp/$q SCORE=0 NOTE=no_result_text"
                continue
            }
            $score = 0
            $note = ""
            if ($resultText -match '(?i)(cannot answer|not.+been built|no data|shard not found|empty results|no graph|TIMEOUT_AFTER|EMPTY_STDOUT)') {
                $score = 0
                $note = "MCP could not answer / timeout"
            } else {
                $hits = ([regex]::Matches($resultText, "(?i)$markersPattern") | Measure-Object).Count
                if ($hits -ge 12) { $score = 9 }
                elseif ($hits -ge 8) { $score = 8 }
                elseif ($hits -ge 5) { $score = 7 }
                elseif ($hits -ge 3) { $score = 5 }
                elseif ($hits -ge 1) { $score = 4 }
                else { $score = 2 }
                $note = "$hits markers"
            }
            $resultLen = $resultText.Length
            $tokens = if ($j.usage.output_tokens) { $j.usage.output_tokens } else { 0 }
            $cost = if ($j.total_cost_usd) { [math]::Round($j.total_cost_usd, 4) } else { 0 }
            $wall = if ($j.duration_ms) { [int]($j.duration_ms / 1000) } else { 0 }
            $turns = if ($j.num_turns) { $j.num_turns } else { 0 }
            Write-Output "$mcp/$q SCORE=$score CHARS=$resultLen TOKENS=$tokens WALL_S=$wall COST=`$$cost TURNS=$turns NOTE='$note'"
        } catch {
            Write-Output "$mcp/$q SCORE=0 NOTE=parse_error: $_"
        }
    }
}
