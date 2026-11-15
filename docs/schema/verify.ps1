# Validates docs/schema: parses every JSON file, checks the schema set is complete,
# evaluates each example against its schema, and re-checks the four hard constraints.
# Dependency-free: PowerShell + Json.NET only.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$failures = New-Object System.Collections.Generic.List[string]

# ---------------------------------------------------------------- JSON parsing
$jsonFiles = Get-ChildItem $root -Recurse -Filter *.json
foreach ($file in $jsonFiles) {
    try {
        Get-Content $file.FullName -Raw -Encoding utf8 | ConvertFrom-Json | Out-Null
    } catch {
        $failures.Add("parse: $($file.Name): $($_.Exception.Message)")
    }
}
"parsed $($jsonFiles.Count) JSON files"

# ---------------------------------------------------------------- tiny validator
function Resolve-Ref($schema, $ref) {
    if (-not $ref.StartsWith('#/')) { throw "unsupported ref $ref" }
    $node = $schema
    foreach ($part in $ref.Substring(2).Split('/')) {
        $node = $node.$part
        if ($null -eq $node) { throw "dangling ref $ref" }
    }
    return $node
}

function Test-Integer($value) {
    if ($value -is [int] -or $value -is [long] -or $value -is [int16] -or $value -is [byte]) { return $true }
    if ($value -is [double] -or $value -is [decimal]) { return ($value -eq [math]::Floor($value)) }
    return $false
}

function Test-Schema {
    param($schema, $value, [string]$path, $root)
    $errors = New-Object System.Collections.Generic.List[string]
    if ($null -eq $schema) { return $errors }
    $names = $schema.PSObject.Properties.Name

    if ($names -contains '$ref') {
        $target = Resolve-Ref $root $schema.'$ref'
        return Test-Schema $target $value $path $root
    }

    if ($names -contains 'type') {
        $t = $schema.type
        $ok = switch ($t) {
            'object' { $value -is [pscustomobject] }
            'array' { $value -is [array] }
            'string' { $value -is [string] }
            'boolean' { $value -is [bool] }
            'integer' { Test-Integer $value }
            'number' { ($value -is [int]) -or ($value -is [long]) -or ($value -is [double]) -or ($value -is [decimal]) }
            'null' { $null -eq $value }
        }
        if (-not $ok) { $errors.Add("$path`: expected $t") }
    }

    if ($names -contains 'const' -and $value -ne $schema.const) {
        $errors.Add("$path`: expected const $($schema.const)")
    }

    if ($names -contains 'enum') {
        $match = $false
        foreach ($allowed in $schema.enum) { if ($value -eq $allowed) { $match = $true } }
        if (-not $match) { $errors.Add("$path`: '$value' not in enum") }
    }

    if ($value -is [string]) {
        if ($names -contains 'minLength' -and $value.Length -lt $schema.minLength) {
            $errors.Add("$path`: shorter than $($schema.minLength)")
        }
        if ($names -contains 'maxLength' -and $value.Length -gt $schema.maxLength) {
            $errors.Add("$path`: longer than $($schema.maxLength) ($($value.Length))")
        }
        if ($names -contains 'pattern' -and -not [regex]::IsMatch($value, $schema.pattern)) {
            $errors.Add("$path`: does not match $($schema.pattern)")
        }
    }

    if (Test-Integer $value) {
        if ($names -contains 'minimum' -and $value -lt $schema.minimum) {
            $errors.Add("$path`: below minimum $($schema.minimum)")
        }
        if ($names -contains 'maximum' -and $value -gt $schema.maximum) {
            $errors.Add("$path`: above maximum $($schema.maximum)")
        }
    }

    if ($value -is [array]) {
        if ($names -contains 'minItems' -and $value.Count -lt $schema.minItems) {
            $errors.Add("$path`: fewer than $($schema.minItems) items")
        }
        if ($names -contains 'maxItems' -and $value.Count -gt $schema.maxItems) {
            $errors.Add("$path`: more than $($schema.maxItems) items")
        }
        if ($names -contains 'items') {
            for ($i = 0; $i -lt $value.Count; $i++) {
                foreach ($e in (Test-Schema $schema.items $value[$i] "$path`[$i]" $root)) { $errors.Add($e) }
            }
        }
    }

    if ($value -is [pscustomobject]) {
        $present = $value.PSObject.Properties.Name
        if ($names -contains 'required') {
            foreach ($field in $schema.required) {
                if (-not ($present -contains $field)) { $errors.Add("$path`: missing required '$field'") }
            }
        }
        $declared = @()
        if ($names -contains 'properties') { $declared = $schema.properties.PSObject.Properties.Name }
        if ($names -contains 'properties') {
            foreach ($field in $present) {
                if ($declared -contains $field) {
                    $sub = $schema.properties.$field
                    foreach ($e in (Test-Schema $sub $value.$field "$path.$field" $root)) { $errors.Add($e) }
                }
            }
        }
        if ($names -contains 'additionalProperties') {
            $ap = $schema.additionalProperties
            foreach ($field in $present) {
                if ($declared -contains $field) { continue }
                if ($ap -is [bool]) {
                    if (-not $ap) { $errors.Add("$path`: unknown field '$field'") }
                } else {
                    foreach ($e in (Test-Schema $ap $value.$field "$path.$field" $root)) { $errors.Add($e) }
                }
            }
        }
    }

    if ($names -contains 'allOf') {
        foreach ($branch in $schema.allOf) {
            foreach ($e in (Test-Schema $branch $value $path $root)) { $errors.Add($e) }
        }
    }

    if ($names -contains 'anyOf') {
        $ok = $false
        foreach ($branch in $schema.anyOf) {
            if ((Test-Schema $branch $value $path $root).Count -eq 0) { $ok = $true }
        }
        if (-not $ok) { $errors.Add("$path`: matches no anyOf branch") }
    }

    if ($names -contains 'oneOf') {
        $hits = 0
        foreach ($branch in $schema.oneOf) {
            if ((Test-Schema $branch $value $path $root).Count -eq 0) { $hits++ }
        }
        if ($hits -ne 1) { $errors.Add("$path`: matches $hits oneOf branches") }
    }

    if ($names -contains 'not') {
        if ((Test-Schema $schema.not $value $path $root).Count -eq 0) {
            $errors.Add("$path`: matches a forbidden shape")
        }
    }

    if ($names -contains 'if') {
        if ((Test-Schema $schema.if $value $path $root).Count -eq 0 -and $names -contains 'then') {
            foreach ($e in (Test-Schema $schema.then $value $path $root)) { $errors.Add($e) }
        }
    }

    return $errors
}

# ---------------------------------------------------------------- examples
$schemas = Get-ChildItem $root -Filter *.schema.json
"schemas: $($schemas.Count)"
foreach ($schemaFile in $schemas) {
    $base = $schemaFile.Name -replace '\.schema\.json$', ''
    $schema = Get-Content $schemaFile.FullName -Raw -Encoding utf8 | ConvertFrom-Json
    $valid = Join-Path $root "examples/$base.valid.json"
    $invalidFiles = Get-ChildItem (Join-Path $root 'examples') -Filter "$base.invalid*.json"
    if (-not (Test-Path $valid)) { $failures.Add("examples: $base has no valid sample"); continue }
    if ($invalidFiles.Count -eq 0) { $failures.Add("examples: $base has no invalid sample") }

    $doc = Get-Content $valid -Raw -Encoding utf8 | ConvertFrom-Json
    $errors = Test-Schema $schema $doc '$' $schema
    if ($errors.Count -eq 0) { "  $base valid   OK" } else { $failures.Add("valid sample rejected: $base :: $($errors -join '; ')") }

    foreach ($invalidFile in $invalidFiles) {
        $doc = Get-Content $invalidFile.FullName -Raw -Encoding utf8 | ConvertFrom-Json
        $errors = Test-Schema $schema $doc '$' $schema
        if ($errors.Count -gt 0) { "  $base invalid OK  ($($invalidFile.Name): $($errors -join ' | '))" }
        else { $failures.Add("invalid sample accepted: $($invalidFile.Name)") }
    }
}

# ---------------------------------------------------------------- hard constraints
function Read-Json($name) { Get-Content (Join-Path $root $name) -Raw -Encoding utf8 | ConvertFrom-Json }

$exec = Read-Json 'execution-view.schema.json'
$commit = Read-Json 'commit.schema.json'
$event = Read-Json 'event.schema.json'
$page = Read-Json 'page.schema.json'
$blob = Read-Json 'blob.schema.json'

$checks = @(
    @{ name = 'executionId maxLength 513 (execution-view)'; ok = ($exec.'$defs'.ExecutionId.maxLength -eq 513) },
    @{ name = 'executionId maxLength 513 (commit)'; ok = ($commit.'$defs'.ExecutionId.maxLength -eq 513) },
    @{ name = 'event kind enum has durable/finalization_started'; ok = ($event.'$defs'.KnownKind.enum -contains 'durable/finalization_started') },
    @{ name = 'event kind enum has durable/finalization_completed'; ok = ($event.'$defs'.KnownKind.enum -contains 'durable/finalization_completed') },
    @{ name = 'page limit minimum 1'; ok = ($page.properties.limit.minimum -eq 1) },
    @{ name = 'page limit maximum 1000'; ok = ($page.properties.limit.maximum -eq 1000) },
    @{ name = '$blob requires digest/bytes/kind'; ok = (($blob.properties.'$blob'.required -join ',') -eq 'digest,bytes,kind') },
    @{ name = '$blob kind is const json'; ok = ($blob.properties.'$blob'.properties.kind.const -eq 'json') },
    @{ name = '$blob rejects extra keys'; ok = ($blob.properties.'$blob'.additionalProperties -eq $false) }
)
foreach ($check in $checks) {
    if ($check.ok) { "constraint OK: $($check.name)" } else { $failures.Add("constraint failed: $($check.name)") }
}

# ---------------------------------------------------------------- summary
if ($failures.Count -eq 0) {
    "RESULT: PASS ($($jsonFiles.Count) JSON files, $($schemas.Count) schemas)"
    exit 0
} else {
    "RESULT: FAIL"
    $failures | ForEach-Object { " - $_" }
    exit 1
}
