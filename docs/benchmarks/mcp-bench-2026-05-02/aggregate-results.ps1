param(
    [string]$ResultsDir = 'C:\Users\team\Desktop\temp\mcp-bench-2026-05-02\results-from-vm'
)

$mcps = @('mneme','tree-sitter','code-review-graph','graphify')
$queries = @('Q1','Q2','Q3','Q4','Q5')

$out = @()
foreach ($mcp in $mcps) {
    foreach ($q in $queries) {
        $f = Join-Path $ResultsDir "$mcp-$q.json"
        if (Test-Path $f) {
            $raw = Get-Content $f -Raw
            try {
                $j = $raw | ConvertFrom-Json
                $row = [PSCustomObject]@{
                    mcp = $mcp
                    query = $q
                    duration_ms = $j.duration_ms
                    duration_api_ms = $j.duration_api_ms
                    num_turns = $j.num_turns
                    cost_usd = $j.total_cost_usd
                    input_tokens = $j.usage.input_tokens
                    output_tokens = $j.usage.output_tokens
                    cache_read = $j.usage.cache_read_input_tokens
                    cache_create = $j.usage.cache_creation_input_tokens
                    is_error = $j.is_error
                    result_chars = ($j.result -as [string]).Length
                    result_first_300 = if ($j.result) { ($j.result -as [string]).Substring(0, [Math]::Min(300, ($j.result -as [string]).Length)) } else { '' }
                }
                $out += $row
            } catch {
                $out += [PSCustomObject]@{ mcp = $mcp; query = $q; error = "parse_error: $_"; raw_first = $raw.Substring(0, [Math]::Min(200, $raw.Length)) }
            }
        } else {
            $out += [PSCustomObject]@{ mcp = $mcp; query = $q; error = 'no_file' }
        }
    }
}

$out | ConvertTo-Json -Depth 5
